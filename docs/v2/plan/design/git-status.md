---
status: current
summary: Sans-IO, hardware-floor git prompt and status engine (`crates/blaze-git`) with its capability-based `bstd::git` driver over `bstd::fs`. Covers everything a Starship-class shell prompt needs (branch, detached HEAD, tag, repo state, ahead/behind, conflicted, staged, modified, deleted, typechanged, untracked, stashed) using batched IO, single-pass `getattrlistbulk` directory walks, zero-copy `.git/index` and binary `commit-graph` parsing, cache-tree subtree pruning, and bounded progress.
date: 2026-11-24
---
# Fast sans-IO Git prompt and status (`blaze-git` + `bstd::git`)

## Goal

Give any blaze/bstd program (such as a Starship-class shell prompt or IDE status bar) a complete,
deterministic, zero-dependency, sans-IO git status engine that runs within 10% of the hardware floor.
No `git` subprocess, no `libgit2`, no `gix`, no `std`, no ambient allocator or filesystem.

## What it computes (Starship feature set)

One call produces a typed `PromptStatus`:

1. **Head (`git_branch`, `git_commit`)**: symbolic branch name (including unborn branches), detached
   commit OID + short hex, optional exact tag match at `HEAD`, worktree kind (`Normal`,
   `LinkedWorktree`, `Bare`).
2. **Repository state (`git_state`)**: `Clean`, `Rebase { interactive, step, total }`,
   `Am { step, total }`, `AmOrRebase { step, total }`, `Merge`, `CherryPick`, `Revert`, `Bisect`.
3. **Upstream & ahead/behind**: branch tracking config from `.git/config` (`[branch "<name>"]`
   `remote` / `merge`), upstream ref resolution (loose + `packed-refs`), and exact symmetric
   commit-graph distance (`Equal`, `Counted { ahead, behind }`, `Capped { .. }`, `Unborn`,
   `UpstreamMissing`, `Skipped`).
4. **Status counts (`git_status`)**:
   - `conflicted`: distinct index paths with stage 1, 2, or 3.
   - `staged`: `staged_added`, `staged_modified`, `staged_deleted`, `staged_typechanged` between
     `HEAD`'s tree and index stage-0 (plus `intent-to-add` entries).
   - `modified`: unstaged tracked files whose bytes or executable mode differ from the index.
   - `deleted`: tracked files missing from the working tree.
   - `typechanged`: tracked files whose working-tree kind (regular vs symlink) differs from the index.
   - `untracked`: non-ignored untracked files (or collapsed untracked directories in `Normal` mode).
   - `stashed`: entry count from `logs/refs/stash` (SIMD newline count).
5. **Prompt formatting (`format`)**: zero-allocation `core::fmt::Write` formatter producing
   Starship-compatible segments with customizable symbols.

## Chosen design vs losing alternative

- **Chosen: Batched sans-IO state machine (`StatusMachine`) + thin `bstd::git` driver.**
  The core (`crates/blaze-git`, `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`) owns
  all git format parsing, ignore matching, cache-tree diffing, commit-graph traversal, and
  stat/racy-git comparison. It emits batched `Want` requests (`ReadFiles`, `MapFiles`, `ListDirs`,
  `StatPaths`, `ReadLinks`, `ReadRanges`) and consumes `Got` replies. The `bstd::git` driver
  executes batches using `bstd::fs::Dir::read_all_many`, `Dir::map_read`, `Dir::entries`
  (`getattrlistbulk` on macOS, `statx` on Linux, `Sim` in DST), `Dir::facts_of`, `Dir::read_link`,
  and `bstd::par` for multi-core SHA-1 verification of dirty/racy files.
- **Losing alternative: Synchronous per-path callback trait (`GitFiles { fn stat(&mut self, ..) }`).**
  Forces serial one-by-one `lstat` trips across the effect layer and prevents unifying directory
  listing (`getattrlistbulk`) with tracked-file statting.

## Hardware floor and cost budget

| Operation | Hardware floor | `blaze-git` + `bstd::git` design |
|---|---|---|
| Clean repo, `HEAD == upstream`, valid cache-tree, `Untracked::Off` | 1 read of `.git/index` + ~15 small control files + `N` stats (or `D` bulk dir reads) | 1 `map_read` of `.git/index`, 1 `read_all_many` for control files, 0 object reads (`root_tree == head_tree` proves `staged == 0`; `head == upstream` proves `ahead == behind == 0`), 1 `Dir::facts_of` (`getattrlistbulk` / `statx`). |
| Clean repo, `Untracked::Normal` | Above + 1 bulk listing per non-ignored dir + 1 read per `.gitignore` | **Single-pass unified walk**: `Dir::entries` returns `Stamp` (size, mtime, ctime, ino, dev, mode) for every entry in one `getattrlistbulk` per directory. Tracked files inside listed directories are verified directly from that stamp with **0 extra `lstat` calls**; only tracked files inside ignored directories are sent to a single batched `StatPaths`. |
| `K` dirty/racy tracked files | `K` file reads + `K` SHA-1 hashes | One batched `ReadFiles` (or `ReadLinks` for symlinks) of the `K` paths, hashed in parallel via `bstd::par` when `K >= 4`. |
| Staged changes in `M` directories | `HEAD` commit + `M` changed tree objects | Full recursive `TREE` cache-tree parsing: any subtree whose cache-tree OID equals `HEAD`'s tree OID is skipped in O(1), reading only the `M` dirty subtrees. |
| Ahead/behind (`HEAD != upstream`, distance `d`) | `d` commit parent lookups | Binary `commit-graph` (`CGPH`, `OIDF`/`OIDL`/`CDAT`/`EDGE` + split `commit-graph-chain`) with generation-number pruning: O(d log N) binary searches, 0 zlib inflates. When no commit-graph exists, reads commits via `blaze-git-fetch::store` (loose + pack `.idx` + range reads) under `Limits::most_commits`. |
| Presence-only mode (`Detail::Presence`) | Stop once dirty/untracked bits are known | Short-circuits hashing and untracked directory walks as soon as each boolean flag is settled. |

## Bounds (`Limits::SHIPPED`)

Every allocation uses the caller's `Alloc<'a>` (`cx.frame()`). Every table, walk, and loop has a
stated cap in `Limits`:

- `most_index_entries`: 4,194,304 (4M)
- `most_worktree_entries`: 4,194,304 (4M)
- `most_dir_depth`: 128
- `most_commits`: 4,096 (ahead/behind walk cap; reports `Divergence::Capped` rather than hanging)
- `most_object_bytes`: 64 MiB
- `most_delta_depth`: 4,095 (git's `pack.depth` limit)
- `most_packs`: 1,024
- `most_ignore_rules`: 65,536

## Proof and testing

1. **Reference model + `blaze_sim::check` property tests (`crates/blaze-git/src/tests.rs`)**:
   - In-memory simulated git repositories (index v2/v3/v4, conflicts, cache-tree subtrees, loose and
     packed objects, `CGPH` commit-graphs, all repo states, `.gitignore` precedence, racy-git,
     symlinks, bare repos, linked worktrees).
   - Equivalence against a simple sequential reference oracle across random repository mutations.
   - **Planted-bug controls** (`GitStatusPlant`): skipping racy-git hash check, skipping cache-tree
     mismatch diff, skipping conflict stage check, ignoring negation in `.gitignore`, skipping
     generation-number stop condition; each caught by a dedicated `the_oracle_catches_*` test.
2. **Deterministic simulation of `bstd::git` (`crates/blaze-std/src/git.rs`)**:
   - Drives `bstd::git::status` over `bstd::io::Io::simulated(SimWorld::new(..))` under `blaze_sim`
     seeds with fault injection and verifies exact agreement with the sans-IO core.
3. **Real-repo validation and floor benchmark** (2026-11-24, macOS arm64, `/Volumes/Projects/blaze`: 16,634 tracked files across 3,428 directories, index v4, split `commit-graph`, 386 commits behind `origin/main`, 131 modified files, 2 stashes, 51 untracked entries):
   - Output matches `git status --porcelain=v2 --branch` on branch (`main`), ahead/behind (`0`/`386`), conflicted (`0`), staged (`0`), modified (`131`), deleted (`0`), typechanged (`0`), stashed (`2`), and untracked (51 vs 48 due to `~/.config/git/ignore` not read by default unless supplied via `StatusMachine::set_global_exclude`).
   - Median latency (5 warm rounds, `--release`):
     - `bstd::git` full Starship mode (`UntrackedMode::Normal`, 3,616 dirs, 0 separate `lstat`s, 0 pack reads): **50.13 ms** (`machine`: 6.78 ms, `read_files`: 1.36 ms, `read_stat`: 1.32 ms, `list_dirs`: 42.97 ms) — **2.08x faster** than `git -c core.untrackedCache=false status --porcelain=v2 --branch` (**104.04 ms**).
     - `bstd::git` `-uno` mode (`UntrackedMode::Off`, `Dir::facts_of`): **29.33 ms** (`machine`: 2.51 ms, `stat_paths`: 24.21 ms) vs `git status -uno` (**26.41 ms**).

