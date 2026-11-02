---
status: plan
summary: Speed pass over the test queue and fork server, the build-script directive parser and elision lexer, and the Windows sandbox, with what each costs per test, per directive line and per sandboxed run before and after.
date: 2026-10-29
---
# Speed of the usercode edge: tests, build scripts, the Windows sandbox

Notes of `speed-link-edge-usercode` (child of `speed-link-edge`), as of 2026-10-29. Code:
`crates/blaze-usercode/src/test/`, `crates/blaze-build-script/`,
`crates/blaze-cargo/src/sandbox/windows.rs`. Nothing here was built or run (the lane's brief
forbids builds); every change has a test beside it that checks the new path against the old.
The figures are counted from the code, not measured.

## What the state of the art does

- **cargo-nextest** runs one process per test, and its per-test cost is a `posix_spawn` plus
  `exec` plus the binary's start. blaze's fork server is the zygote pattern instead (Android's
  zygote, Chromium's): the test binary starts once, then forks per test, so a test costs a fork
  and a few syscalls, with no `exec` and no dynamic loading.
- **Bazel persistent workers** keep one process per tool and speak framed requests on a pipe.
  The fork server speaks the same kind of framed protocol (LE `u32` length, kind byte), and now
  writes one batch per poll turn, as a worker writes one response per request.
- **memchr** finds lines with SIMD (SSE2/AVX2/NEON), one pass over the bytes. The directive
  parser, the elision lexer and the build script's log now find lines and quotes with it, rather
  than by `str::lines` with per-line UTF-8 work.
- **Bazel's Windows sandbox** and other AppContainer launchers use job objects and keep ACL
  grants instead of rewriting them per action. blaze keeps read grants. The per-run write grant
  is the one remaining per-run ACL rewrite (see the proposal below).

## Per test (fork server and queue)

Parent side of the fork server, per test:

| | before | after |
|---|---|---|
| syscalls | socketpair, fork, setpgid, pidfd_open or kevent, ioctl, close, poll, reads, waitpid ×2, one `write` per frame | the same, but one `write` per poll turn for every frame of the turn |
| allocations | two `Vec`s per poll turn, the output copied twice, a `Vec` per encoded frame | none in steady state: the poll sets, the outgoing buffer and the children's output buffers are reused (up to 1 MiB each) |

Queue side (`test/queue.rs`), per test:

- Before:
  - every record line was cloned into a `String`, and its scope cloned again as a hash-map key;
  - each record chunk was copied by `drain(..).collect()`;
  - the server's common lines (empty scope) were cloned once per child that reported them;
  - each completion ran a linear scan of `pending`;
  - one `Effect::Send` went out per frame;
  - job names were looked up with SipHash on a `String`.
- After:
  - lines are parsed in place, and only kept lines allocate (one `Line`);
  - common lines are deduped by a per-name bitmask of the kinds seen;
  - scheduling pops from a `BTreeSet` of ready tests (O(log n));
  - a chunk is adopted without a copy when nothing is waiting;
  - each call sends each server one `Effect::Send`, then its timers;
  - a job is found by its index (`job_index`), with no name lookup.
- What is still paid per test: the common lines are cloned into each test's `Trace::Lines`,
  and `trace::assemble` hashes them. A request in `v2/requests/usercode.md` would let
  `assemble` take any iterator of `&Line`, so a trace can share them with an `Arc`.

Tests:

- `test/tests.rs`:
  - an `Ended` frame encoded from borrowed parts has the frame's bytes;
  - frames are read in place exactly as from a whole stream, and `needed()` is exact.
- `test/queue/tests.rs`:
  - one Send per server per call, each timer after it;
  - a common line is given once;
  - a non-UTF-8 line reads as the lossy record does;
  - the in-place line reader agrees with `Line::parse`, a seeded differential test.

## Per directive line (`blaze-build-script`)

`output.rs`, parsing a build script's stdout:

| | before | after |
|---|---|---|
| line search | `str::lines` over UTF-8-checked text | memchr for `\n` |
| a line that is not a directive | UTF-8 check and `trim` | rejected on its first non-space byte |
| reserved key | scan of 17 prefixes | one `match` |
| messages | `format!` of the package's description and origin on every parse | `Display` wrappers, formatted only when an error is made |
| value | `replace` of the old `OUT_DIR` on every value (an allocation) | a copy only when the value names it |
| allocations per kept line | 3 to 4 | 1 (the kept value) |

Tests: `a_directive_after_any_space_is_read`, `a_line_is_a_directive_exactly_when_cargo_reads_one`
(differential against the old reading) and `a_reserved_key_is_one_of_cargo_s_prefixes`.

The elision lexer (`elide/lex.rs`):

- strings borrow from the source until their first escape (`Cow`);
- line comments, raw strings, `\r` and the text-direction code points (lead byte `0xE2`) are
  found with memchr;
- integers are folded with `checked_mul` instead of parsed from a `String`;
- `elide.rs` formats into one `String` by byte index.

Its tests check text direction against every `char`, check that a string borrows until an
escape, and check integers against `str::parse`.

`run.rs` `Ran::logged`: the log is split with memchr, and only the message after
`cargo::error=`, `cargo:warning=` or `cargo::warning=` is decoded. Before, the whole output was
lossily decoded and split by line. Its test is a differential against the old lossy-lines model.

## Per sandboxed run on Windows (`sandbox/windows.rs`)

Take R read paths (all granted by an earlier run) and W write directories holding F files.

- **Before:**
  - 2R stats (`exists`, then `is_dir`) and R DACL reads, all while holding the session-wide
    named mutex, so every launcher of the session waits on every other's checks;
  - then, for each write directory, a stat, a DACL read and a DACL write that propagates to all
    F files below it, repeated on revoke.
- **After:**
  - R stats and R DACL reads, done without the lock;
  - the lock is taken only for the write grants and for any read grant still missing, which is
    re-read under the lock before it is changed;
  - write directories skip the stat (`create_dir_all` just made or found each one).
- **Test:** a read that is seen covered without the lock is one a grant would not add to, for a
  directory and for a file.

Proposed, not applied (in `v2/requests/usercode-sandbox.md`, section 6):

- A write capability derived from the directory, so write ACEs stay between runs and no run
  rewrites or propagates a DACL. This changes the sandbox's security argument.
- `CreateAppContainerProfile` only when the profile is missing.
- Running the launcher inside the daemon, so a run costs one `CreateProcessW` instead of two.
  That needs the lead and the driver.

## Unsafe

These paths follow [unsafe.md](../../design/unsafe.md):

- `blaze-build-script` forbids `unsafe_code`.
- The fork server and the Windows sandbox use unsafe only to reach the kernel. Each is a module
  with a safe API (`serve`; `run` and `probe`).
- Every block in them is one operation with a `// SAFETY:` comment.
- None of the speed changes above added `unsafe`. The SIMD is the `memchr` crate's, and each
  use is checked against a scalar model in tests.

## Left as it is

- `Env` stays a `BTreeMap` of about 100 variables per run: `blaze-usercode` relies on its
  type, and it costs microseconds against a millisecond process start.
- The queue's tag and attempt maps stay `BTreeMap`s. A slab would reuse ids, which the driver's
  tag contract forbids, and a stale deadline timer could then hit a reused attempt. About 100 ns
  per test, against a fork.
