# Common brief for norustc-refused class agents (blaze, step E of docs/no-rustc.md, issue #85)

Repo github.com/indexable-inc/blaze (Rust compiler + daemon `ra-jit`, sold as `blaze`). Main
checkout /Volumes/Projects/blaze-enable-1026. Read AGENTS.md first (rules: no-rustc, no-fallback,
best-long-term-design, heavy work on a dev host). Lane HANDOFF with the refusal inventory:
/Volumes/Projects/blaze-norustc-refused/HANDOFF.md (read the "Refusal table").

## What we know
- `cargo blaze build` is always the whole build now: every crate compiled by blaze over the
  standard library's unit (`ra-jit std`, which compiles every function of std's crates as a root).
  A function blaze refuses becomes a stand-in (hir-ty-codegen/mod.rs `define_refused`) and a build
  whose link still reaches a stand-in fails (crates/ra-jit/src/cargo/whole.rs).
- On the bench apps (ripgrep, helix, nushell) nothing is refused beyond the std unit; every
  refusal is in std's unit. Self-hosting (blaze building blaze) reaches crc_fast's x86 intrinsics.
- The LLVM backend (hir-ty-codegen/llvm.rs) lowers the *Cranelift IR* the front half makes, so the
  front half (MIR -> Cranelift IR in hir-ty-codegen/mod.rs) refuses the same functions under both
  backends. Fixes go in the front half.
- Refusals are `CodegenError::Unsupported` via the `unsupported!` macro in hir-ty-codegen/mod.rs.

## How to work
- Your own worktree: `git -C /Volumes/Projects/blaze-enable-1026 fetch origin && git -C
  /Volumes/Projects/blaze-enable-1026 worktree add /Volumes/Projects/blaze-nrr-<x> -b nrr-<x> origin/main`.
- Builds and tests run ONLY on dev host dev-aws-14 (`ssh -p 9999 andrew@100.86.201.88`), already
  claimed for lane norustc-refused. Use your own dir /dev/shm/nrr-<x> there (clone
  https://github.com/indexable-inc/blaze works from the host; push your branch from the Mac and
  `git fetch` it on the host, or rsync your worktree over a host clone). Other lanes use
  /dev/shm/nrr, /dev/shm/selfhost, /dev/shm/norustc-std: don't touch them.
- Long commands: `systemd-run --user --collect --unit nrr-<x>-<step> -p CPUWeight=20
  /run/current-system/sw/bin/bash <script>` with `export PATH=/run/current-system/sw/bin:$HOME/.cargo/bin:$PATH`
  inside, output to a log file; never a foreground ssh build over a few minutes. Never `pkill -f`
  over ssh. The host's login shell is zsh: pipe scripts with `ssh host bash -s < file`.
- Build: `cd src && CARGO_TARGET_DIR=/dev/shm/nrr-<x>/target nix develop --command cargo build -p ra-jit --release --features llvm --locked` (~70 s cold).
- The std unit's refusals: `RA_JIT_SKIPPED=<file> ra-jit std <out-dir> "$(rustc --print sysroot)"`
  with RUSTUP_TOOLCHAIN=$(cat harness/toolchain), RA_JIT_HOME and RA_JIT_CACHE set to fresh dirs,
  inside `nix develop`. It writes `function: reason` per refused function. Count before and after.
- Check against rustc: add cases to `harness/corpus` (one module per area; `python3 harness/difftest.py
  corpus --bin runner --crate cases --layouts` compares blaze's output with rustc's). Also run
  `harness/allrajit.py` if your change touches the std unit, and whatever other harness in
  docs/harness.md fits. x86 cases must also cover what the host CPU lacks only if they can run: the
  host is AMD Zen 4 with AVX-512 (dev-aws-14 is a 192-vCPU AMD host; check /proc/cpuinfo).
- Write code in the repo's style: prose comments in full sentences saying why, as the surrounding
  code does; no fallbacks (AGENTS.md no-fallback). Don't write big test suites: corpus cases that
  compare with rustc are the check. Don't edit docs/no-rustc.md's other steps; update docs/support.md's
  "Known refusals" table if your class is listed there.
- Land: small PR, one class. `gh pr create` against main with a title like the repo's commit
  titles and a body saying what was refused, what compiles now (the std-unit counts before and
  after), and how it was checked. Wait for the merge gate (~4 min, `gh pr checks <n> --watch`);
  when green, `gh pr merge <n> --admin --merge` (authorized). If the gate fails, read the failure
  and push a fix. After merge, delete your host dir and worktree.
- Session restarts kill agents: keep /Volumes/Projects/blaze-nrr-<x>/HANDOFF.md (untracked)
  current after every step, and read it first if it exists.
- Final reply: PR URL(s) merged, std-unit count of your class before -> after, what's left.
