# nocargo lane handoff (2026-10-27)

Brief: /tmp/blaze-lanes/nocargo.txt. Goal: `cargo blaze build`/`test` of harness/norustc/fixture
and ripgrep exec no cargo (beyond the typed `cargo blaze` itself).

## State
- Worktree /Volumes/Projects/blaze-nocargo-1027, branch nocargo-1027 off origin/main 47793534d2.
- VM `nocargo` (arm64, 32 cpu, 32G committed, ttl 12h). Tree at /root/nc/src, release build
  `nix develop -c cargo build -p ra-jit --release` (unit nc-build, log /root/nc/build.log).
- Trace scripts: /tmp/nocargo-scripts/{trace.py,classify.py} on the Mac, copied to /root/nc/.

## Step 1: measurement (done, 2026-10-27, main 47793534d2, VM nocargo arm64)

`strace -f --seccomp-bpf -e trace=execve,clone,clone3,fork,vfork` of `cargo blaze build` and
`cargo blaze test` from a clean `target/`, with and without RA_JIT_WHOLE_BUILD=1, daemon stopped
before each run and after it went quiet (scripts: /tmp/nocargo-scripts/trace.py, classify.py;
JSON in /tmp/nocargo-scripts/r1). Counted: execs of the toolchain's real cargo (the rustup proxy
in front of it is not counted twice). All 8 runs exit 0.

| caller | asks | from | fixture-build-whole | fixture-build | fixture-test-whole | fixture-test | ripgrep-build-whole | ripgrep-build | ripgrep-test-whole | ripgrep-test |
|---|---|---|---|---|---|---|---|---|---|---|
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | unit load | 2 | 4 | 2 | 2 | 14 | 14 | 18 | 20 |
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | workspace load | 2 | 2 | 2 | 2 | 2 | 2 | 4 | 4 |
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | ? | 0 | 0 | 0 | 0 | 2 | 2 | 4 | 4 |
| `vendor/rust-analyzer/crates/project-model/src/cargo_config_file.rs CargoConfigFile::load` | config get --format toml --show-origin | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/version.rs get` | --version | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/rustc_cfg.rs rustc_print_cfg` | rustc --print cfg | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/target_data.rs get` | rustc --print target-spec-json | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `crates/ra-jit/src/plan.rs UnitGraph::of` | build --unit-graph | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `crates/ra-jit/src/plan.rs Flags::of` | config get --format json | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/build_dependencies.rs build_command (main.rs workspace_at)` | check --compile-time-deps | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `(typed) cargo blaze` | the user's command | - | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| **cargo total** | | | 12 | 14 | 12 | 12 | 26 | 26 | 41 | 43 |

Reading it:
- RA_JIT_WHOLE_BUILD no longer exists in the code: build/run/test always take `whole.rs`. Both
  columns are the same path; differences are run-to-run (how many unit loads ran).
- "unit load" = ra-jit loading a `rust-project.json` it wrote to compile one build script or proc
  macro (own_script.rs/own_macro.rs, under `--compile-time-deps`). Each load asks cargo for the
  sysroot's `library/` metadata twice (`--no-deps`, then full). "?" = the same from a sandboxed
  probe whose parent strace could not see.
- Biggest caller by far: sysroot.rs `load_library_via_cargo` (2 per load, every load): 28 of
  ripgrep test's 43.
- No `cargo fetch`/`generate-lockfile`: blaze-registry fetches already. No workspace
  `cargo metadata`: blaze-metadata answers (OwnMetadata) for every load measured.
- rustc execs in the raw trace are `target/.../ra-jit-rustc/rustc` stand-ins for build-script
  probes (anyhow, proc-macro2, libc, serde: `--version` and probe compiles), not the toolchain's.

Per workspace load: config get toml, --version, rustc --print cfg, rustc --print
target-spec-json, sysroot metadata x2, build --unit-graph, config get json, check
--compile-time-deps = 9, plus 2 per unit load.

## Step 2: removal, in parallel sub-lanes (started 2026-10-27 ~02:15 PDT)
Shared brief: /tmp/blaze-lanes/nocargo-sub-common.md. Each has worktree
/Volumes/Projects/blaze-nocargo-<sub>-1027, branch nocargo-<sub>, VM nc-<sub>, own HANDOFF.md.
- `sysroot`: sysroot.rs load_library_via_cargo (2 per load, biggest) -> blaze-metadata.
- `toolq`: config get toml/json, --version, rustc --print cfg / target-spec-json; workspace.rs
  "so cargo does" fallbacks -> errors.
- `unitgraph`: plan.rs UnitGraph::of -> pure unit graph in cargo's schema (#458-shaped).
- `ctdeps`: --compile-time-deps -> blaze's own build-script/proc-macro driver over the unit graph
  (depends on unitgraph to land the switch).
Parent: wait for them, re-trace after merges (r2), update docs/no-rustc.md, write
/tmp/blaze-lanes/nocargo.result.md. Parent VM `nocargo` still up for re-traces.

## Restart 2026-10-27 02:20 (watchdog)
Previous run died with its 4 sub-agents. Their worktrees had uncommitted WIP (sysroot: hook + harness
written, not yet built/run; toolq: 8 files; unitgraph: new crates/blaze-units started (schema.rs);
ctdeps: plan.rs started) and their VMs nc-{sysroot,toolq,unitgraph,ctdeps} still RUNNING. No PRs yet.
Re-spawned one agent per sub-lane at ~02:25 to resume from its worktree WIP + VM (each writes its
own HANDOFF.md and commits WIP often).
Re-spawned (02:25): sysroot, toolq, unitgraph, ctdeps agents running in background. If this run dies
again, check each sub worktree's HANDOFF.md and `git log origin/main..` before re-spawning.

## Restart 2 (2026-10-27 02:32, watchdog)
Agents died again (the parent's run ending kills background subagents). State found: sysroot
built + harness 26/26 equal, next after-trace + land; toolq 3 commits, next build/test/land;
unitgraph 3 WIP commits (blaze-units crate, blaze-metadata units()), no HANDOFF.md; ctdeps 1 commit
+ uncommitted WIP (compile_time.rs etc). VMs nc-* all RUNNING. No PRs yet.
Re-spawned all 4 as FOREGROUND agents in one message (parent blocks until they return), so the
parent stays alive. If restarted again: same check, same re-spawn.

## Restart 3 (2026-10-27 02:37, watchdog)
State: sysroot PR #477 open, auto-merge on, CI running (14->8 / 43->15 cargo execs; VM removed).
toolq 4 commits (built? next: test, resolve.py toolchain, measure, land); unitgraph 6 WIP commits
(blaze-units + resolve.py units part); ctdeps 4 WIP commits. VMs nc-{toolq,unitgraph,ctdeps} RUNNING.
Re-spawned toolq, unitgraph, ctdeps as foreground agents (one message). sysroot: parent watches #477.

## Restart 4 (2026-10-27 03:45, watchdog)
State found: #477 sysroot MERGED. #485 toolq: VM step5 green (toolchain 35/35, rustcsites ok,
tests ok) -> queued with land.py --no-wait (03:50). #516 unitgraph: CI failed on rustcsites (the
units example ran rustc for host cfg) and stale nix/units. Parent fixed it on nocargo-unitgraph
(668afdff73: example takes --host/--host-cfg-file from resolve.py; sites.json recorded), rebased on
main 56cb5bd311; VM nc-unitgraph2 unit ug-r7 (/root/ug/r7.log): nix/unit_graph.py, cargo test
blaze-units/metadata, resolve.py units. Then: copy nix/units back (/root/ug/r7-units.tar), commit, push.
ctdeps: harness 1/14 equal (10 bench blaze-failed, error text truncated to "ce:"+backtrace);
re-spawned ctdeps agent (background, 03:55) to fix driver/harness and land; VM nc-ctdeps.
- 04:02 #485 toolq MERGED. #516 became unmergeable after it: rebased nocargo-unitgraph on main
  babd08e169 (resolve.py/resolve.md: kept both toolchain and units parts; no-rustc.md: combined
  text, table has @FB@/@FT@/@RB@/@RT@ placeholders for the combined counts, fill from the trace).
  VM nc-unitgraph2 unit ug-r8 (/root/ug/r8.log): nix/unit_graph.py (-> /root/ug/r8-units.tar),
  build, rustcsites, trace (/root/ug/out/after8), resolve.py units toolchain. Then commit units,
  fill placeholders, force-push (lease on 64d9cb4893's predecessor at origin), land.py again.
- 04:30 #516 rebased on #485, fixed (plan::Asked keeps cfg), r8 trace on combined tree:
  fixture 2/2, ripgrep 2/3 cargo execs (left: check --compile-time-deps + typed). units 13/14,
  toolchain 35/35. Pushed + re-queued with land.py. nix/units regenerated in VM.
  Next: ctdeps agent lands its PR (must rebase on #516); then parent re-traces main (VM nocargo).
- 04:31 #516 unitgraph MERGED; nc-toolq and nc-unitgraph2 removed. Waiting on ctdeps agent
  (background; if dead, re-spawn from /Volumes/Projects/blaze-nocargo-ctdeps-1027/HANDOFF.md).
  After it lands: re-trace main in VM nocargo, update docs/no-rustc.md self-contained text, result.md.


## Restart 5 (2026-10-27 06:13 PDT, watchdog)
ctdeps agent dead (last commit 05:50). Parent now drives ctdeps itself in its worktree (no
background agents). h9 harness (unit ct-h9, nc-ctdeps, /root/ct/h9.log) was still running.
Left for ctdeps: h9 result, cargo test -p ra-jit -p project-model, rustcsites, after-counts,
docs placeholders COUNTS_TABLE/CTDEPS_RESULTS, git rm --cached HANDOFF.md, squash, land.
- 06:40 ctdeps squashed + rebased on main b6a385c8de (conflicts with bscache #585 keyed builds in
  own_script.rs resolved: build_described -> built(); links_proc_macro passed to build()). Backup
  branch nocargo-ctdeps-prerebase. Commit 2: said() uses answers::own_answer_of only (rustcsites
  flagged answers::answer, which can spawn rustc).
- h9 (old base): ctdeps 10/14 equal, nushell+biome blaze-failed (#327 syn-1 proc macro codegen,
  pre-existing), zed+bun both-failed (cmake/ld/path pkgs missing).
- r3 trace (VM nocargo, rebased tree): ALL 8 runs = 1 cargo (typed cargo blaze only), 0 toolchain
  rustc/rustdoc (the rustc execs are ra-jit-rustc stand-ins = ra-jit). ripgrep test: 320 of 329
  integration tests fail NotFound (`../rg`) since #569 (release lane: matches pinned nightly cargo's
  second layout, where cargo fails the same). Not this lane's.
- 06:25 VM nocargo died (FAILED, unreachable) during nc-r4; removed. arm64/x86 had no room for
  32G committed; now VM nocargo3 (arm64 16 cpu 16G committed), same /root/nc layout, unit nc-all
  (/root/nc/all.sh: setup, build, server, r4 trace, rustcsites, cargo test; log /root/nc/all.log).
  Ship with `VM=nocargo3 bash /tmp/nocargo-scripts/ship.sh <worktree>`.
- h10 first result: fd different (tikv-jemalloc-sys OUT_DIR build/config.log) after the rebase.
- 06:40 nc-ctdeps also died (FAILED); h10 lost; removed. All work now on nocargo3.
- 06:55 r4 (nocargo3, final tree 2 commits): all 8 runs 1 cargo (typed), rustcsites ok (13, none
  new). project-model not a workspace member (cargo test -p fails); running unit nc-t2: cargo test
  --release -p ra-jit, then LIMIT=100 /root/ct/harness.sh (full ctdeps harness), log /root/nc/t2.log.
- 07:10 cargo test --release -p ra-jit: ok (279). fmt ok. Full harness (rebased, sandbox on):
  9/14 equal. fd different = SHELL withheld by main's script sandbox env filter
  (script_sandbox.rs `given`, from #585) -> configure's config.log says SHELL=/bin/sh; with
  BLAZE_SANDBOX=off fd 9/9 packages equal. helix blaze-failed = sandbox withholds the bench's
  HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1, script fetches grammars, writes into its source, refused;
  with the env + BLAZE_SANDBOX=off helix 38/38 equal. -> bscache lane's (report, don't fix).
  zed: my strict said() hit `--print cfg -C symbol-mangling-version=v0`: fixed in answers/own.rs
  (commit 3, IGNORED_CODEGEN). nushell now equal; biome still #327.
- Running nc-h11 (/root/nc/h11.log): rebuild, answers:: tests, harness sandbox on
  (/root/ct/h11-on.json) and off (/root/ct/h11-off.json). Then: fill resolve.md CTDEPS_RESULTS,
  squash (drop HANDOFF.md), land.py, result.md.
- 07:25 nocargo3 died too (h11 lost), removed. PR #658 opened (land.py --no-wait, auto-merge),
  body /tmp/nocargo-scripts/658-body.md. Rebased again on main bcb5f9577e (closure/fetches mods,
  build_nothing `shipped` arg), + Cargo.lock (ra-jit -> blaze-build-script; CI --locked failed).
  Head 8470a70e6a. Backup branches nocargo-ctdeps-{prerebase,prelandsquash,pre2}.
  nix/units NOT regenerated: nix/unit_graph.py fails on arm64 on main too ("two units both named
  libc custom-build ... aarch64-darwin"); no x86_64 capacity. "nix unit graphs are current" is not
  required (only `ci`). Report it.
- VM nocargo4 (arm64 16/16G commit) unit nc-all (/root/nc/all.log): r5 trace, rustcsites, cargo
  test -p ra-jit, harness on (/root/ct/on.json) and off (/root/ct/off.json) on the rebased tree.
- 07:52 #658 MERGED (ci green). r5 on final tree: 8/8 runs 1 cargo. ctdeps harness on merged
  tree (sandbox on): 10/14 equal (helix now equal; fd SHELL only, equal with sandbox off by pair).
  Docs follow-up #682 queued (auto-merge). nocargo4 removed; sub worktrees removed (handoffs
  copied to /tmp/nocargo-scripts/handoffs). Result written: /tmp/blaze-lanes/nocargo.result.md.
  LANE DONE. Only #682 left to merge (check `gh pr view 682`).
- (old) Running: nocargo unit nc-r4 (build, cargo test -p ra-jit -p project-model, rustcsites, r4 trace,
  /root/nc/r4.log); nc-ctdeps unit ct-h10 (rebased build, srv, full harness, /root/ct/h10.log).

## Operator note 2026-10-27 11:33Z (Andrew: build scripts and proc macros are cached, and hermetic)
DECISION: compiling a build script, running it, and building a proc macro are each ordinary store entries,
keyed by what they are built from (sources, deps, cfgs, env the script declares or is recorded reading, target,
toolchain), and they travel through the remote (rcache) like any compiled function. Goal from lane warmstate: on a
fresh machine whose store was pulled, this step is ~0 s. Today it is 4.9 s of a 7 s no-edit ripgrep build (70%).
- Build-script runs are hermetic by default, the same deny-by-default sandbox as tests (lane testcache, one
  sandbox implementation, not two): no network (netns with loopback only), executables only from recorded inputs
  (the toolchain, the C compiler, pkg-config as the operator allows), writes only to OUT_DIR, reads recorded.
  The recorded reads and env lookups go into the key, so `rerun-if-*` is a hint, not the truth.
- A denial fails the build with a clear error naming the script and what it tried. No silent fallback to an
  unsandboxed run or to cargo. Opt-out is explicit per package (manifest/config), and opted-out runs are not
  cached remotely.
- Measure first: over the bench corpus, list which build scripts the sandbox would deny (network fetches,
  stray executables, reads outside the package) and put the list in the PR.
- Proc-macro dylibs are content-addressed by their inputs; a hit loads the stored dylib with no compile.
- Report the warmstate fresh-none number before and after.
- ~05:30 ctdeps agent (background) on run 5: rebased on #516, harness h8 running in nc-ctdeps
  (nushell/biome blaze-failed, helix anyhow cfgs differ; fd/cargo/ripgrep/bat equal by hand).
  Final re-trace plan (VM nocargo): replace /root/nc/src with main's tree, bash build.sh,
  server.sh, ROUND=final measure.sh (traces fixture/ripgrep build/test, --whole too).

## From release (2026-10-27 14:17Z)
The train's `harness (*, buildmodel)` (all platforms) stays red on case `dep_replace`: "`[replace]` is not read
by blaze yet" (blaze-metadata; since #516 the daemon's unit graph comes from blaze-metadata). It passed at
07d08dab6e via cargo's unit graph. Please read `[replace]` (or say it is refused by design, and the release lane
will make the case expect a refusal). Release's fixture fix for git-index registries: PR #641 (merged).

## Note from lane `nightly` (2026-10-27 16:10Z)
install.yaml's "no Rust installed" jobs (Debian/RHEL/Ubuntu) fail at the first `blaze run` of the
hand-written lockless serde project: "blaze could not plan the build: blaze picks a registry
package only at the version Cargo.lock has (#321): no matching package named `serde` found"
(blaze-metadata/src/lib.rs:632). Seen on #543 (run 40498240832); toolchain-independent
(blaze-resolve, no cargo/rustc). main's install workflow has not completed since 10-26 11:22Z, so
it is red on main for any PR touching install.sh. Yours (#321) if you own blaze-metadata's
registry; nightly lane does not touch it.
