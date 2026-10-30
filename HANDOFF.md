# borrowck lane (2026-10-27) — HANDOFF

**2026-10-27 09:47Z (operator): a separate lane `bkperf` now owns borrowck SPEED** (tmux
blaze-bkperf, brief /tmp/blaze-lanes/bkperf.txt, HANDOFF /Volumes/Projects/blaze-bkperf-1027/HANDOFF.md).
It works in the same crate (timing mode, data structures in regions/liveness/loans/polonius).
Keep your fixes to rule changes; if one needs a structural rewrite, note it in both HANDOFFs.

Brief: /tmp/blaze-lanes/borrowck.txt. Worktree /Volumes/Projects/blaze-borrowck-1027, branch borrowck-1027.
VM: `borrowck` (arm64, 32 GiB committed, ttl 12h; nodes had at most ~43 GiB free, and the first
VM OOMed growing on demand). In VM: /root/bk/blaze (tree), /root/bk/rust (rust tests @ pin
12433c30), /root/bk/target (CARGO_TARGET_DIR), /root/bk/apps (app checkouts), logs /root/bk/*.log,
scripts /root/bk/{setup,ui,apps,all}.sh. Release build of ra-jit takes ~75 s on 128 cores.

## Baseline at origin/main 47793534 (pin nightly-2026-10-25), measured 2026-10-27 in VM

- Polonius 1544/1584, NLL 1541/1582 (docs said 1545/1585, 1542/1583 at f9db2381, older pin).
- No unit rustc accepts gets an error (both modes). No panics.
- Trap: running both modes with --jobs 24 on a fresh VM gave a bogus Polonius run (900 units
  "rust-analyzer reports E0425/E0282"): the rust-src component was being auto-installed under
  the concurrent first runs. Run once warm, or polonius alone after.
- 9 units are out of scope because rust-analyzer reports a type error rustc doesn't
  (6 E0282, 2 syntax-error, 1 E0277): step-4 material.
- harness/borrowck_apps.py (new, this branch): `ra-jit borrowck --project` over apps.json + corpus + mixfutures.

## Failing units grouped by root cause (40 Polonius; NLL = same + nll-problem-case-3#nll)

A. derive on #[repr(packed)] moves out of a packed field (E0507 missed) — 3 units
   derives/deriving-with-repr-packed{,-move-errors,-no-clone-suggestion}.rs
B. place expression statement not read (`S[0];`, `*s;`) — 2 units (MIR lowering)
   dst/dst-index-fail.rs (E0507), unsized/issue-115203.rs (E0508)
C. well-formedness of items/signatures with projections (E0309/E0310/E0311 missed) — ~12 units
   asm/inline-asm-with-lifetimes#bad, async-await/in-trait/async-generics{,-and-bounds},
   dyn-compatibility/supertrait-mentions-GAT, implied-bounds/gluon_salsa#{old,next},
   implied-bounds/sod_service_chain, impl-trait/in-trait/async-and-ret-ref,
   traits/next-solver/opaques/implied-bounds-leak-hidden-ty-3#{current,next},
   implied-bounds/from-trait-impl, generic-associated-types/bugs/issue-87735,
   nll/user-annotations/normalization-infer (E0310 + E0716)
D. higher-ranked subtyping / placeholders — ~7 units
   async-await/witness-auto-trait/higher-ranked-auto-trait-11#no_assumptions,
   borrowck/implementation-not-general-enough-ice-133252, nll/closure-malformed-projection-input-issue-102800,
   nll/snocat-regression (forall-outlives), implied-bounds/normalization-placeholder-leak#fail,
   generic-associated-types/bugs/hrtb-implied-2 (E0521), closure-expected-type/expect-fn-supply-fn
E. bodies in dead code not checked — nll/nested-bodies-in-dead-code (E0597 x3, lifetime)
F. closures / async closures captures — async-closures/without-precise-captures-we-are-powerless
   (7 missed), borrowck/borrowck-multiple-captures (EXTRA E0382 at line 49: a false error)
G. impl Trait member constraints / blame — impl-trait/precise-capturing/migration-note (E0515 vs E0597 @184),
   impl-trait/in-trait/cannot-capture-intersection (E0700), impl-trait/precise-capturing/rpitit,
   impl-trait/wf-check-hidden-type#{current,next}, fn/suggest-return-closure (E0597),
   async-await/async-borrowck-escaping-closure-error (E0515)
H. right error, wrong line/code — fn/fn_def_coercion (19 vs 20), wf/wf-fn-def-check-sig-1 (10 vs 11),
   nll/closure-requirements/propagate-multiple-requirements (16 vs 15),
   cast/ptr-to-trait-obj-different-regions-misc (E0521 vs lifetime @46)
I. misc — associated-types/unconstrained-lifetime-in-projection (E0716),
   nll/polonius/nll-problem-case-3-issue-124254#nll (E0503 missed, NLL only)

## Status
- [x] step 1 UI baseline
- [ ] step 1 apps sweep (running: unit bk-all, /root/bk/apps.log)
- [ ] fixes via subagents

## Run 2 (restart 02:07 PDT)
- Apps sweep (bk-all unit) was still running at restart; 16 apps so far 0 errors 0 panics.
- Orphans: worktrees blaze-bk-c-1027, blaze-bk-d-1027 (no commits), VM bk-c (fresh). Reused by new subagents.
- Subagents spawned (brief /tmp/blaze-lanes/borrowck-sub.md, handoffs /tmp/blaze-lanes/borrowck-<g>.md):
  F (false error first), C, D, G, AB (A+B), EHI (E+H+I).
- Apps sweep done (main 47793534): 20/23 clean. False errors: nushell 53 (E0716, mostly tests), ruff 19
  (3 E0716 ruff_formatter printer, 14 "lifetime may not live long enough" + 2 E0621 in ty_python_semantic),
  rust-analyzer 1 (E0515 hir-def expr_store/lower.rs). Full list /tmp/blaze-lanes/borrowck-apps-false-errors.txt.
  Subagents: T (E0716 temporaries), L (ruff lifetimes + r-a E0515). These come first.
- PR #465 (borrowck_apps.py + docs) queued auto-merge.

## Run 3 (restart 02:22 PDT)
- Subagents of run 2 died. State found: F -> PR #470 (auto-merge queued), EHI -> PR #469 (E only, queued;
  uncommitted regions.rs WIP for H/I in worktree). Uncommitted WIP in worktrees c, d, g, t, l; ab none.
  All bk-* VMs still running. #465 merged.
- Respawned subagents (resume from worktree WIP + existing VM): T, L, C, D, G, AB, EHI(H+I).
- Run 3 subagents spawned 02:25 (each resumes its worktree WIP, own VM, writes /tmp/blaze-lanes/borrowck-<g>.md):
  t, l (app false errors, priority), c, d, g, ab, ehi (H+I on new branch bk-h-1027).
- Next for parent: when PRs merge, rerun UI suite + apps sweep on main in VM `borrowck`, update docs/borrowck.md
  numbers table, write /tmp/blaze-lanes/borrowck.result.md. Then step 4 (r-a diagnostics) if time.

## Run 4 (restart 02:37 PDT) — runs get killed ~every 15 min; subagents now WIP-commit + write handoff early
- Merged: #469 (E), #470 (F), #465. Queued auto: #474 (C partial: 3 units, +3 both modes), #475 (D partial: hrtb-implied-2, +1; tango check failed, not inspected: gh rate limit).
- Removed VMs bk-f (done), bk-c (FAILED). bk-l STOPPED.
- WIP uncommitted at restart: c (on top of #474), d (on top of #475), ehi (H/I, branch bk-h... WIP commit "fndef variances"), g, l, t. ab: nothing.
- Respawned subagents t, l, c, d, g, ab, ehi with "commit WIP every step" rule.
- 02:40 spawned 7 subagents (t, l, c->bk-c2-1027, d->bk-d2-1027, g, ab, ehi->bk-h-1027). Shared resume rules
  /tmp/blaze-lanes/borrowck-resume.md; each writes /tmp/blaze-lanes/borrowck-<g>.md as it goes.
  On next restart: read those files, respawn only unfinished groups with the same prompts (resume rules + group task).
- Parent still to do: after PRs merge, rerun UI both modes + apps sweep (harness/borrowck_apps.py) on main in VM
  `borrowck`, update docs/borrowck.md numbers, write /tmp/blaze-lanes/borrowck.result.md.
- 02:45 bk-main unit in VM borrowck: all.sh on origin/main 40e72e14 (logs /root/bk/{main,ui,apps}.log, ui.json)
- main 40e72e14 (with #469 E, #470 F): Polonius 1546/1584, NLL 1543/1582, 0 errors on units rustc accepts, no panics. Apps sweep running in bk-main (/root/bk/apps.log).
- Apps on main 40e72e14: still 20/23 clean, same false errors (nushell E0716, ruff E0716+lifetimes, r-a E0515): owned by subagents t and l.
- 02:53 G done: PR #490 queued (main 5441c2c8: Pol 1550->1558, NLL 1547->1555). Left: general member-constraint pick recheck.
- 03:02 H+I done: PRs #481 #486 #487 #500 #501 queued (all 6 units). Read rustc from May nightly src, not the pin.
- 03:13 D done: #480 #484 #499 #509 queued (stacked on #475; tango on #475 = bench noise). 4/6 units.
  Left: higher-ranked-auto-trait-11#no_assumptions, implementation-not-general-enough-ice-133252 need coroutine
  witness types (r-a coroutine_hidden_types stub, hir-ty next_solver/interner.rs:2150) = async-block Send checking.
  Also: old prove() path maps 'x: placeholder to always-true where rustc wants 'x: 'static (latent missed error).
- 03:35 AB done: #505 (A: borrowck expands builtin derives, packed copies) + #515 (B: unsized temporaries). 5 units once both land.
  RISK: #505 makes `ra-jit borrowck --project` check derived impls on real code -> must rerun full apps sweep after it merges.

## Run 5 (restart 03:46 PDT, after session-limit stop)
- Merged since run 4: #505 (A derives), #497 #498 (C), #499 #484 #480 (D), #481 #486 #487 #500 #501 (H+I).
- OPEN without auto-merge: #515 (B), #509 (D5), #490 (G, CONFLICTING). Need requeue / rebase.
- Unfinished: T (qualifs promotion, OOM investigation, no PR), L (3 fixes on bk-l/bk-l2/bk-l3, no PRs; RA lower.rs:702 = cause C alias elision),
  C (bk-c-annot-1027 normalization-infer, val8 running in bk-c).
- VMs alive: borrowck, bk-t, bk-l, bk-c.
- 03:50 requeued #509 (#515 already queued). Spawned subagents G (rebase #490), L (open PRs A/B/C), T (OOM then PR), C (annot PR then rest).
- 03:52 VM borrowck FAILED (host memory-cgroup OOM of the VMM during 128-core build). Recreated with --cpus 32 --memory 32
  --memory-commit; scripts now in /tmp/bk-parent/all.sh (Mac copy). Unit bk-main3: main 07d08dab build + UI both modes
  (/root/bk/ui-{polonius,nll}.txt) + apps (borrowck_apps.py, /root/bk/apps.log, apps.json). This is the post-#505 sweep.
- 04:04 VM borrowck OOM-killed AGAIN at 32G/32 cpus (host cgroup kills the VMM mid-build). Now 16 cpus, 16G committed
  (ix only takes 4/8/16/32/...), build jobs 8, UI jobs 8, apps jobs 1 without ruff (ruff needs >16G). Unit bk-main4.
- main 07d08dab: NLL 1561/1582 (lane start 1541), 0 errors on accepted. Polonius run bogus (cold rust-src, 900 E0425/E0282 OOS) -> rerun.

## Operator note 2026-10-27 11:17Z
The release train's `fork alone` job is red again on c5cb7804 (run 40494685771): it runs the rust-analyzer
fork's tests, incl. ide-diagnostics borrowck fixtures whose expectations your rule fixes change. Before
queuing each borrowck PR, run the fork's ide-diagnostics + hir-ty-borrowck tests and update fixtures in the
same PR. Check the failing tests in that run and fix them now; the `release` lane is blocked on it.

## Operator 11:22Z: the fork-alone failures are handled by lane `forkalone`; follow its note on running fork tests before queuing.
- 04:25 apps sweep main 07d08dab (all but ruff): clean except cargo 1 (NEW regression, cargo-credential error.rs:88
  `err as &dyn StdError` in closure; suspect #487), rust-analyzer 1 (group L), nushell 53 (group T; 1 is E0515
  nu-plugin-engine context.rs:344, check whom), bun 77 (bun NEW in apps.json 52eace591d, first measurement).
  Full list /tmp/blaze-lanes/borrowck-apps-false-errors-07d08dab.txt. #505 derive expansion caused no new app errors seen.
- G done: #490 rebased+requeued (main 56cb5bd3: P 1563->1571, N 1561->1569). C done (#519 + handoff lists left units).
- Spawned O (object-lifetime default in body annotations; cargo + bun error.rs E0521), M (bun move errors ~45),
  R (bun region errors ~20). Brief /tmp/blaze-lanes/borrowck-apps-sub.md, handoffs /tmp/blaze-lanes/borrowck-{o,m,r}.md.
- Still running from run 5: T, L. On restart: read borrowck-{t,l,o,m,r}.md; respawn unfinished with same prompts
  (resume rules + group task as described in those files).
- nushell E0515 context.rs:344 = `&Signals::EMPTY` (assoc const promotion) -> group T.
- 04:28 C finished: #518 (normalization-infer, +1) queued, #519 merged (leak-hidden-ty-3, +1). Left in C: gluon_salsa
  (wfcheck needs normalized sigs with fresh regions: inference ctx in hir-ty-wfcheck), issue-87735 (after that),
  supertrait-mentions-GAT (unfixable: dummy-span internal obligation). VM bk-c removed.
- G finished: #490 rebased, queued (main 56cb5bd3: P 1563->1571, N 1561->1569). VM bk-g removed.
- #518 had no auto-merge after subagent; queued by parent 04:29.

## Note from lane `nightly` (2026-10-27 ~12:00Z; /Volumes/Projects/blaze-nightly-1027/HANDOFF.md)
The pin moves to nightly-2026-10-27 (rustc 16d1ffac1) through a PR from branch `toolchain/bump`,
opened by the new daily workflow (PR #529) and merged only when the train on that branch has no
job/harness red that is green on main and the std unit refuses no more fns than main
(harness/stdcount.py). After it merges, from then on daily: re-fetch rustc tests at the new commit
(pins.txt `commit`), rebuild ra-jit with the new nightly, and expect `rustc +nightly-<date>` verdicts
(borrowck_ui.py cache keys on the toolchain name, so old caches are not reused). ra-jit builds on
10-27 with no source changes. If your merged work changes what the bump gates on (borrowck
--min-pass floor, std refusal buckets), the bump re-runs on top of main daily; nothing to do.
A failure the bump causes in your area shows as a comment on the bump PR and the issue
"Toolchain bump to <date> is blocked"; fixes go on branch toolchain/bump.

## forkalone (lane `forkalone`, 2026-10-27; /Volumes/Projects/blaze-forkalone-1027/HANDOFF.md)

- `fork alone` went red from #499 (higher-ranked fn pointer subtyping): a false E0308 on minicore's/core's
  `fmt::rt::Argument::new_display` (`Self::new(x, crate::fmt::Display::fmt)`), caught only by ide-diagnostics
  `tests::minicore_smoke_test`. Cause: the reify-to-`for<'a> fn` path turned RA's ReError for `Formatter<'_>` into a
  root-universe variable; fix makes it an anonymous bound var of the item's binder. PR #546
  (https://github.com/indexable-inc/blaze/pull/546).
- BEFORE QUEUING EACH borrowck PR, run the fork's tests as the job does, WITH `CI=1` (minicore_smoke_test and the
  other slow tests skip themselves without `CI` or `RUN_SLOW_TESTS`, so a plain run passes while CI fails):
  `cp -r vendor/rust-analyzer vendor/rustc_type_ir vendor/rustc_next_trait_solver vendor/rustc_abi vendor/salsa
  vendor/salsa-macros vendor/salsa-macro-rules $T/ && cd $T/rust-analyzer && CI=1 cargo test -p hir-ty
  -p hir-ty-borrowck -p hir-ty-wfcheck -p ide-diagnostics && cargo check -p hir` (in your VM, under `nix develop`).
- 04:50 O done: #547 (body &dyn object default = fresh region; cargo 1->0). Main 6709adac: P 1569/1584, N 1567/1582.
  bun error.rs E0521 x6 are NOT that cause: `<&'static str>::from(e)` on strum IntoStaticStr derive (bun errno crate);
  errors_assign.rs:1719 x6 = group L (Self in dyn FnMut). O resumed on the strum E0521s.

## Note from lane `bkperf` (05:20, structural, no rule change)
- Coming PR (branch bkperf-intervals): hir-ty-wfcheck/src/outlives.rs `Inferred` becomes a thin per-check memo over
  a salsa query `inferred_outlives(db, AdtId)` (fixpoint via cycle_fn/cycle_initial), replacing the in-struct
  in_progress/cyclic/fixing logic. `Inferred::of` keeps its signature. If you change `compute`/`required` rules,
  edit the free fns `inferred_outlives`/`required` after it lands. UI both modes: 0 units differ.
- Also: hir-ty-borrowck Ctx::is_copy now caches per body (key: type with free regions -> 'static).
- #545 (queued): interval sets, SCC inference, block-wise loan scopes in regions.rs/liveness.rs/loans.rs/polonius.rs/
  check.rs/inference.rs/intervals.rs. Rebase on it if you touch those.
- FYI apps check on main f99b49d6: cargo, rust-analyzer, nushell, ruff each report false errors (same on #545).
- 04:55 O2: bun's 77 were mostly ARTIFACTS: bun checkout lacked vendor/lolhtml + vendor/rust-argon2 -> cargo metadata failed
  -> r-a silently fell back to --no-deps graph. Real bun list (main a6d5388e): 36 errors,
  /tmp/blaze-lanes/borrowck-bun-full-graph-a6d5388e.txt: clone_types.rs:767 E0716 x12, hir_dump.rs x8 lifetime,
  errors_access.rs x6 lifetime, errors_assign.rs:1719 x6 (L), parse_entry.rs E0515 x2, transpiler.rs:1605 E0515, pe.rs:428 E0793.
  Notice appended to borrowck-apps-sub.md / borrowck-{m,r}.md (M and R were on the artifact list; check their PRs are real bugs).
- O3 running: ra-jit refuses failed-metadata workspace (no-fallback) + borrowck_apps.py prepares bun's vendor deps.
- 05:10 M done: #553 (offset_of! on packed is not a reference, bun pe.rs E0793). bun move errors were all missing-deps artifacts.
- L done: #557 (A Self in dyn/fn ptr), #558 (B closure &dyn 'static default), #559 (C elision vs aliases/dyn); r-a 1->0.
  ruff full run NOT done at current main (no >=16G committed capacity). ruff ty_site_packages lib.rs:1263 `e as &dyn Error`
  in closure = likely #547 (O). bun errors_assign.rs:1719 x6 = likely #557.
- Remaining bun (35, full graph): hir_dump x8, errors_access x6 (R), clone_types E0716 x12 (R or T), E0515 x3 (R).
- Parent TODO: once #547 #553 #557-559 #490 #515 #518 + T merge: sweep main incl. bun full graph (after O3 harness) + ruff if 32G.

## Operator note 12:15Z
main merged into bk-g-1027 (#490) by the operator; docs/borrowck.md conflict combined both texts. Pull before pushing to it.
- 05:22 T done: #579 (promotion qualifs port; nushell 53->0, ruff_formatter 3->0, corpus case). No memory blowup:
  earlier VM deaths = --jobs 16 x 1.4 GiB/process on 16G, 128-cpu default, shared CARGO_TARGET_DIR stale artifacts.
  UI at main 0226a955: P 1569/1584, N 1567/1582 (unchanged by #579).
- Still running: R (bun region errors), O3 (no-fallback metadata + bun prep in harness).
- 05:23 R done: #570 MERGED (ra-jit borrowck --project refuses failed cargo metadata; borrowck_apps.py fetches bun vendor deps
  via bunprep.sources, bun no longer skipped) and #580 (promote &mut [] via index/method autoref; bun clone_types E0716 x12).
  O3 told to stand down (dup of #570). Merged since: #515 #547 #557. Queued: #490 #518 #579 #580.
- Expected bun after queue: errors_assign/hir_dump/errors_access x20 -> #557; parse_entry E0515 x2 -> #579; pe.rs -> #553;
  left: transpiler.rs:1605 E0515 (unanalyzed). Next: final sweep on main once queue drains.
- gh pr merge --auto sometimes errors "--merge/--rebase/--squash required" (merge-queue detection flake): use GraphQL enablePullRequestAutoMerge. #580 queued that way.
- 05:34 O3 done: #583 (unit test for refusal; bunprep applies bun's rust-argon2 patch). bun at main a3dd1ae1: 15 errors
  (12 clone_types -> #580, 2 parse_entry -> #579, 1 transpiler.rs:1605 unanalyzed). For other lanes (result file):
  ra-jit main.rs workspace_at (build/daemon path) still accepts r-a's --no-deps fallback after cargo metadata fails.
- 05:37 X spawned (bun transpiler.rs:1605 E0515). Unit bk-ui5 in VM borrowck: UI both modes on main cea88f8a -> /root/bk/ui-{polonius,nll}.txt
- 05:42 UI main cea88f8a: Polonius 1577/1584, NLL 1575/1582, 0 errors on accepted, no panics. Failing (same both modes):
  higher-ranked-auto-trait-11#no_assumptions + implementation-not-general-enough-ice-133252 (coroutine witness, D: separate project),
  supertrait-mentions-GAT (unfixable: dummy span), issue-87735 + gluon_salsa#{old,next} (C2 spawned), normalization-infer (#518 queued).
- Spawned C2 (bk-c3-1027, gluon_salsa/87735) and S4 (step 4: 9 OOS units with r-a type errors E0282 x6, syntax x2, E0277 x1).
- Running: X, C2, S4. Final: when #518 #580 #583 + these land, apps sweep on main (all apps incl bun; ruff if 32G) + docs numbers + result.
- 05:58 X done, no PR: transpiler.rs:1605 E0515 already fixed by #579 (assoc const promotion). bun on main cea88f8a = 12 (all clone_types E0716, fixed by #580). bun -> 0 once #580 merges.

- 13:05Z (lane conflicts): merged origin/main (6eb8f30c39) into bk-c-annot-sq-1027 (#518); regions.rs kept both zip_annotation arms; UI 1578/1576 vs main 1577/1575; pushed, auto-merge on. Pull before pushing to it.

## Run 6 (restart 06:13 PDT)
- Merged: #518 #579 #583 #490. #580 was CONFLICTING (docs/borrowck.md promotion paragraph): merged main into bk-r2-1027
  in worktree blaze-bk-r-1027, combined both texts, pushed; auto-merge still on.
- Respawned C3 (gluon_salsa/87735, VM bk-c3) and S4 (r-a invented type errors, 4-branch stack, VM bk-s4).
- Parent next: sweep main (UI both modes + apps incl. bun) in VM borrowck; final docs numbers + result file.
- 06:20 UI at main 9bf0+#580 (bk-r2-1027 92a95750): Polonius 1578/1584, NLL 1576/1582. Failing (both modes): coroutine
  witness x2 (separate project), supertrait-mentions-GAT (dummy span), issue-87735 + gluon_salsa x2 (C3). Apps sweep
  running in unit bk-main6 (/root/bk/apps.log; NOTE systemd file: does not truncate, old tail text remains).
- 06:23 VM borrowck rebooted mid apps sweep (after websvc; first 14 apps clean). Rerun of the rest: unit bk-apps7, /root/bk/apps2.{log,json}.
- 06:33 APPS on main+#580 (#580 now MERGED): 22/22 apps + corpus + mixfutures clean, 0 panics (bun 0, nushell 0, cargo 0, r-a 0).
  ruff not in the list: no 32G capacity (best node 6.8 GiB free). Trying ruff in VM borrowck under MemoryMax=14G (unit bk-ruff, /root/bk/ruff.log).
- 06:37 ruff (2010 files) clean, 0 panics, 7.2G peak under MemoryMax=14G: ALL 23 apps clean.
- 06:40 Spawned K (step 4 on real code: new harness cargo blaze check vs cargo check over apps; worktree blaze-bk-k-1027,
  branch bk-k-1027, uses VM `borrowck` (tree /root/bk/blaze-k); handoff /tmp/blaze-lanes/borrowck-k.md).
  Parent now has VM bk-k (8G, only size available) for the final UI rerun after C3/S4 land.
  On restart: respawn unfinished of C3, S4, K with resume rules + their handoff files.
- 06:55 UI at main cb12bbeb (VM bk-k, 8G, jobs 4): Polonius 1580/1584, NLL 1578/1582 (gluon_salsa landed via C3 PR1).
  Left: coroutine witness x2, supertrait-mentions-GAT, issue-87735 (C3 PR2). bk-k Polonius first run was cold-sysroot bogus; rerun valid.
- 07:00 VM borrowck stopped again under K's first run; K restarted it (ix start), disk kept. K harness check_apps.py WIP on bk-k-1027. K finding: blaze's check emits errors only, never warnings -> every rustc warning is 'missed' (step 4 design question).
- 07:20 bk-s4 STOPPED (S4 owns it); C3 now on VM bk-c3b; K pilot 0 invented/0 missed errors, full sweep unit k2 in VM borrowck.
- 07:09 K force-restarted VM borrowck (hung during k2 sweep); K resumes it. VM borrowck is unstable under load (3rd death today).
- 07:15 C3 DONE: #618 merged (gluon_salsa), #646 queued (issue-87735 + normalizes_to_error rule). Expected after #646: P 1581/1584, N 1579/1582. VMs bk-c3/bk-c3b removed.

## Note from lane `bkperf` (2026-10-27 ~14:35Z)
PR #651 (branch bkperf-prove) restructures `Builder::prove` in regions.rs: the solver call moved into
`prove_canonical` -> salsa `prove_query` (canonical, keyed by env + trait ref with ReVars numbered), with the old
direct path kept as the `None` arm. Rule changes inside prove (what is done with pairs/obligations) are unaffected;
a change to HOW the goal is proven must go in both `prove_query` and the direct arm. UI identical both modes.
Planned next, NOT started, to sequence with you: implied_bounds as a per-signature query (your WF/implied-bounds
work is in flight there, so I will not touch it without a note here first).
- 07:47 S4 DONE: #668 (E0282 tainted), #669 (E0369/E0368 binop), #670 (E0525), #671 (fresh_binding) all target main,
  each contains the lower layers: land in order. Top of stack: P 1580/1584 -> 1585/1590, N 1578/1582 -> 1583/1588.
  Left from S4: pat-at-same-name-both E0384 (MIR: StorageDead guard bindings on guard failure; borrowck-lane), 116599
  (hir-ty inference of `_` in qualified path trait args), 2 syntax units (parser: &pin, Unicode confusables). VM bk-s4 removed.
- 07:50 #646 MERGED. Spawned P (E0384 guard bindings, pat-at-same-name-both; worktree blaze-bk-p-1027, VM bk-k or bk-p; handoff /tmp/blaze-lanes/borrowck-p.md). Running: K, P.
- 08:21 P DONE: #680 queued (guard failure ends pattern bindings; drops mutate_place binds exception). main ed7168c8 P 1586/1590 N 1584/1588 -> 1587/1590, 1585/1588. Left in UI: coroutine witness x2 + supertrait-mentions-GAT. Trap: shared CARGO_TARGET_DIR across trees reuses stale vendor crates. Running: K.


## Note from lane `nightly` (2026-10-27 17:05Z)
main is pinned to nightly-2026-10-27 since 16:53:36Z (#543, merge 250fc25bee; rustc 16d1ffac1). Its verdict:
std refusals 1940 vs 1940 (+0), no train job red that is green on its base. That merge (GitHub auto-merge for
GITHUB_TOKEN) started no push workflow; I dispatched on main at 16:58Z: ra-jit, train (40545629890), fmt,
bench, nix-live, bench-readme, install, pgo, site, fork-sync. #699 makes the verdict do that itself. Next bump:
scheduled 2026-10-28 05:40Z, PR from toolchain/bump, merges only when its gates are green on top of main.
- 10:25 K DONE: #653 (harness/check_apps.py), #693 (macro expr-position `e;` expansion), #704 (reified fn item &dyn default) all MERGED. 14 apps 0 invented/0 missed errors. Final sweep unit bk-final in VM borrowck on main 68be5dec (target-final, logs /root/bk/final-*). bk-k removed.
- 11:12 final sweep main 68be5dec: Polonius 1587/1590 (3 known left). NLL run bogus (pin moved to nightly-2026-10-27 -> cold). cargo app HUNG 37 min in ra-jit build-script (build-rs-test-lib build_script_build.real; at 06:25 it took 58s) = NEW, build-script area (other lane); killed pid. UI tests still at rust commit 12433c30 (old pin).
- 11:30 FINAL main 68be5dec: Polonius 1587/1590, NLL 1585/1588, 0 errors on accepted, apps 24/24 clean (all incl bun, ruff).
  Docs PR #715 (numbers + What is left) queued. Result written to /tmp/blaze-lanes/borrowck.result.md. VM borrowck removed.
  LANE DONE except #715 merging. Leftover subagent worktrees blaze-bk-*-1027 can be removed (all their PRs merged).

## Note from lane `bkperf` (2026-10-27 ~18:40Z)
Branch bkperf-ph changes `prove_query` in regions.rs: an answer that names placeholders of a `for<'a>` (or
variables of non-root universes) is no longer `Proven::Here` (re-proven in the body each check) but carried
with its universes numbered 1..n, which the body creates fresh in order (rustc's canonical query response).
What prove does with pairs/obligations is unchanged. Validated by the UI suite in both modes before it lands.

## Triage tri-a, 2026-10-28 ~17Z: issues routed to this lane
- #84 (no-rustc D): the false errors on nushell/ruff (#480, #481, #484) are the borrow-check part still open.
