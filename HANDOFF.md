# borrowck lane (2026-10-27) — HANDOFF

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
