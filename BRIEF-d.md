# Brief for one bevy frontend refusal (lane norustc-refused, agent d's sub-agent d<N>)

Read first: /Volumes/Projects/blaze-norustc-refused/BRIEF-common.md (host, builds, landing) and
AGENTS.md in your worktree (rules: no-rustc, no-fallback, best-long-term-design,
semantics-in-rust-analyzer). Your <x> is `d<N>`:
- worktree: `git -C /Volumes/Projects/blaze-enable-1026 fetch origin && git -C /Volumes/Projects/blaze-enable-1026 worktree add /Volumes/Projects/blaze-nrr-d<N> -b nrr-d<N> origin/main`
- host dev-aws-14 (`ssh -p 9999 andrew@100.86.201.88`), your dir /dev/shm/nrr-d<N> only. Long commands via
  `systemd-run --user --collect --unit nrr-d<N>-<step> ...` writing a log, never a long foreground ssh.
- Keep /Volumes/Projects/blaze-nrr-d<N>/HANDOFF.md (untracked) current after every step; if it exists when you
  start, a previous run of you was killed: continue from it.

Context: bevy v0.20.0's `cargo blaze build --example breakout` refuses some functions (the stand-in reaches
the link and the build fails). The bevy tree is /dev/shm/nrr/inv1/ws/bevy on the host (read-only: copy what
you need). Dependency sources are in ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ on the host
(wesl-0.6.0, symphonia-*-0.5.5, bevy_asset is in the bevy tree under crates/). A baseline ra-jit built from
origin/main (with blaze-proc-macro-srv) is at /dev/shm/nrr-d/bin/ (read-only; wait for "SERVER RC" in
/dev/shm/nrr-d/setup.log if it is still building). Refused functions are listed with RA_JIT_SKIPPED=<file>
(one `function: reason` line each) or in the worker log under RA_JIT_HOME.

Your one bug is below. Do:
1. Reproduce it, then reduce it to a small self-contained case (a tiny crate, no deps) that ra-jit refuses
   the same way. Find the root cause; don't paper over it.
2. Fix it properly, the way rustc does it (port rustc's rule). Language semantics (inference, MIR lowering,
   layout) go in vendor/rust-analyzer; codegen in crates/hir-ty-codegen. No fallbacks, no special-casing the
   crate.
3. Add the reduced case to harness/corpus (docs/harness.md "Adding a corpus case"; or harness/breakers if it
   can't be a corpus function) and run `python3 harness/difftest.py corpus --bin runner --crate cases --layouts`
   on the host: it must print MATCH. Run the vendored rust-analyzer crate's own tests you touched
   (e.g. `cargo test -p hir-ty` for the module) if you changed r-a. Also confirm the original function now
   compiles (rebuild the real crate, e.g. a small crate depending on wesl 0.6.0 / symphonia 0.5.5 / bevy_asset
   by path, or the bevy tree copy) and say what, if anything, it refuses next.
4. Land one PR: `python3 .github/scripts/land.py "what the branch does"` in your worktree (AGENTS.md
   land-through-queue). Body: what was refused, root cause, the fix, how checked. If the gate fails, read
   the failure and push a fix.
5. Clean up: delete your host dir, worktree and branch after merge.

Final reply (short): PR URL and whether merged, root cause in two lines, whether the original function now
compiles, anything it refuses next, anything left.
