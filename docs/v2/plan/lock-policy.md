---
status: plan
summary: lock policy (lane lock-policy, 2026-11-04, approved by Andrew) - CI refuses any change to the lock files, naming each move, why and the fix; locally a build writes them but never silently; `blaze.lock` beside `Cargo.lock` pins the blaze release and its nightly, the index's newest publish time and every pinned native input's digests; `lock = required` refuses locally too and blaze's tree sets it; what is built and what is left.
date: 2026-11-04
---
# Lock policy

Lane lock-policy, 2026-11-04. The design Andrew approved that day, and what this lane built of
it. The user-facing description is docs/cargo.md, "Lock files"; this file says why and what is
left.

## The rules

1. **CI refuses.** With `CI` set (not empty, not `false`, not `0`) or `TF_BUILD` set, `--locked`
   is the default. A missing lock file, or a resolve that would change it, is an error naming
   each package added, removed or moved (old → new), why (which requirement of which package
   asked for it), and the fix (`blaze lock` locally, then commit). `--no-locked` opts out.
2. **Locally, never silent.** With no lock file, blaze resolves, writes `Cargo.lock` and says one
   line: `wrote Cargo.lock: 143 packages, index as of 2026-11-04T10:12Z (commit it)`. A change
   forced by a manifest edit says what moved, and stays minimal: the resolve keeps every
   locked package the manifests still allow, as cargo's does.
3. **The lock covers everything blaze pins:** the blaze release, the nightly it ships
   (`harness/toolchain`), every pinned native input's digests (`[workspace.metadata.blaze.native.*]`)
   and the index's snapshot time. Pinned inputs that disagree with the lock are a lock change.
4. **Opt-in strict mode:** `[workspace.metadata.blaze] lock = "required"` refuses locally too.
   blaze's root `Cargo.toml` sets it.

The decision (`crates/blaze-units/src/lock.rs`, `policy`), first row that applies:

| Asked | A change to a lock file |
|---|---|
| `--locked`/`--frozen` and `--no-locked` | error naming both flags |
| `--locked`/`--frozen` | refused |
| `--no-locked` | written, said |
| CI | refused |
| `lock = "required"` | refused |
| otherwise | written, said |

## Where the pins live: `blaze.lock`

A second file at the workspace root, `blaze.lock`, not `Cargo.lock`'s `[metadata]`:

- `Cargo.lock` stays byte for byte what cargo writes. cargo, `cargo metadata`, cargo-deny,
  cargo-audit, Dependabot and Renovate read and rewrite it; anything blaze adds would be theirs
  to drop or keep.
- cargo's `[metadata]` is a flat map of strings that `cargo update` copies through untouched. A
  pin there would go stale without a word the first time a person ran cargo, which is the silent
  drift this policy exists to stop. `blaze.lock` is written only by blaze, so what it says is what
  blaze last checked.
- Nested, typed tables (`[native.llvm.targets]`) read and diff better than encoded strings.
- One home for every pin that is not a crate (`one-concept-one-home`). #864's networked build
  steps (a step's output hash) belong here as `[step.<id>]` tables when they are built, not in a
  third file.

The format (version 1; unknown keys and other versions are errors naming the file, never
guessed at):

    # blaze.lock: what blaze builds this workspace with besides the crates in Cargo.lock.
    # blaze writes it (`blaze lock`); commit it. docs/cargo.md, "Lock files".
    version = 1

    [blaze]
    release = "0.20261104.0"
    toolchain = "nightly-2026-10-27"

    [index]
    newest = "2026-11-04T10:12:00Z"

    [native.llvm]
    pin = "crates/hir-ty-codegen/llvm.json"

    [native.llvm.targets]
    aarch64-apple-darwin = "e95fc3b4…"

Tables and keys are sorted, so the bytes depend on nothing but the pins
(`deterministic-output`); equality is structural, so a hand-edited spacing is no change.
A lock that would pin nothing is not written.

What each entry means, and the choices made:

- **`[blaze]`** is recorded only when the running blaze is a release, it runs as the workspace's
  pin says (the pin names this release, or a channel, or nothing), and `--ignore-pin` was not
  given for this workspace. A build of blaze's tree (`0.0.0-dev`) keeps whatever is there: it
  is not a release anyone can fetch, and recording it would flip the lock between dev and
  release builds of the same tree.
- **`[index] newest`** is the newest sparse-index `pubtime` among the packages the resolve took
  from an index. The load is a pure function of its inputs and owns no clock
  (`sans-io-cores`), so it cannot know when the index was fetched; the newest publish time it
  used is a deterministic lower bound of that, and is what "index as of" says. A resolve that
  took nothing from the index (everything locked) keeps the old value.
- **`[native.*]`** covers every input the root manifest declares and every target its pin file
  names, not only what this build needs, so the lock does not depend on the host or the
  packages built.

## Built (this lane)

- `crates/blaze-units/src/lock.rs`: the policy, `blaze.lock`'s model, render and parse, the move
  lines with their reasons (`moves`, `pin_moves`), and `decide`, which turns the two files'
  changes into nothing, a write with what to say, or the refusal. Its two tests: the decision
  table, and `blaze.lock` round-tripping byte for byte.
- `crates/blaze-units/src/load/resolve.rs` (`lock_files`): both files after the resolve, read
  through the load's snapshot, decided as above; a refusal is the load's error at `Cargo.lock`.
- `crates/blaze-resolve/src/lockfile.rs`: `lockfile_change`, the text without the policy (the
  old `lockfile_to_write` stays for blaze-metadata and its tests).
- `crates/blaze-units/src/load/native.rs`: `locked_pins`; reading a pin now checks every
  target's name and digest, not only the host's.
- `crates/blaze-units/src/load/invocation.rs`: `--no-locked`, `CI`/`TF_BUILD` (read apart from
  the variables cargo reads, since cargo's `CI` changes `incremental`), the running release
  (`BuildConfig::blaze`).
- `crates/blaze-units/src/query.rs`: `LockWrite` is the files to write and the lines to say.
- `crates/blaze-cargo/src/serve/job.rs`: writes each file and says each line as a note.
- `crates/blaze-cargo/src/cli/args.rs`: `--no-locked`, and `blaze lock` as `check --no-locked`.
- Root `Cargo.toml`: `lock = "required"`; `blaze.lock` with the LLVM pin, written by hand from
  `crates/hir-ty-codegen/llvm.json` in the format `render` writes.

## Left

- **The launcher does not honour `blaze.lock`.** A workspace pinned to a version runs it; one
  pinned to a channel, or unpinned, runs whatever release it gets, and a release other than the
  lock's can only be refused (CI) or recorded (locally). Next: the launcher reads `[blaze]
  release` for a channel or unpinned workspace and runs that release, as it does a version pin.
- **`blaze lock` builds.** It is `check --no-locked`, so it checks the workspace too. It should
  stop after the load (a plan-only request to the daemon).
- **Index fetch time.** "index as of" is the newest publish time used, not when the index was
  read. Recording the fetch time needs the sources layer to hand the load a timestamp as an
  input, keyed, which it does not.
- **Unpinned workspaces under CI** are refused after every blaze upgrade, since the recorded
  release changes. Intended (the lock says what built it), but a workspace that wants to float
  should pin a channel once the launcher honours the lock.
- **blaze's tree at the next pin bump.** `blaze.lock` holds no `[blaze]` yet: the pinned
  0.20261030.1 predates this policy and dev builds record none. The first pin bump to a release
  with it must run `blaze lock` and commit `blaze.lock`, or every build of the tree is refused.
- **Harnesses and e2e that build throwaway workspaces with no lock file under CI** are now
  refused; each passes `--no-locked` or commits a lock. Not swept in this lane.
- **`lock::is_release` duplicates blaze-engine's `pin::is_version`.** They should be one function
  in a crate both depend on.
- `blaze-metadata`'s commands (`remove`, `vendor`) still take `--locked` only, not CI or
  `lock = "required"`.
