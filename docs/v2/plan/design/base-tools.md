---
status: plan
summary: The one pinned set of tools a sandboxed build step may run, so nothing comes from the machine unless it has to (D192).
date: 2026-11-21
---
# Base tools

A build script, a deferred C compile, a process tree (`sh configure`, `make`) and a native script
module run in a sandbox. D192: what they run and read is pinned, the same bytes on every machine.
Today (interim) clang and the LLVM tools are pinned on the shelf, and every other tool is an OS
path granted per package in `pins/clang.json` `host_tools`. That table goes.

## The set

One archive per platform, named by its digest in `pins/base-tools.json`, fetched once to the
toolchain shelf beside clang. Its `bin` folder is the whole `PATH` of a sandboxed step.

| Group | Programs |
|---|---|
| C toolchain (pinned today) | clang, clang++, lld, llvm-ar, llvm-nm, llvm-ranlib, llvm-strip, llvm-objcopy |
| Shell and core | sh, cat, cp, mv, rm, rmdir, mkdir, ln, ls, chmod, touch, expr, test, true, false, echo, printf, basename, dirname, sort, uniq, tr, head, tail, wc, cut, tee, env, uname, hostname, date, sleep, find, xargs, install, mktemp |
| Text | sed, grep, awk, diff, cmp |
| Build drivers | make, cmake, ninja, pkg-config |
| Script languages | perl, python3 |
| Archives | tar, gzip, xz, zstd, unzip |
| Generators | protoc; nasm and yasm (x86-64 only) |

Not in the set: `git` (answered from recorded facts, never run: a real git reads the whole
repository and its configuration, which no key covers).

## What still comes from the machine

A short list, each with its identity in the key of every step that used it:

- The Apple SDK (headers and `.tbd` stubs): not redistributable. Identity: version and
  `ProductBuildVersion` (`SdkId`).
- The kernel, `dyld` and the system libraries a process must load (`libSystem`). Identity: the
  OS build.

Anything else a step asks for that is not in the set is the error a machine without that tool
gives (`sh: cmake: command not found`), named with the package and the step. Never a reach to
the OS.

## Rules

- Determinism: `uname`, `hostname` and `date` answer fixed values (the target's, `localhost`,
  `SOURCE_DATE_EPOCH`); the environment is the one the script captured plus the pinned
  variables; `TZ=UTC`, `LC_ALL=C`.
- The key of a step holds the set's digest, not each program's. A new set reruns every step
  that ran a program; a step that ran none (a pure wasm script) does not hold it.
- The Seatbelt profile grants read and execute on the set's folder and nothing under `/bin`,
  `/usr/bin` or an Xcode folder.
- Built once by CI, statically linked, from pinned sources. A user never needs the tool that
  built it.

## Order of work

1. The reader: `pins/base-tools.json`, the shelf folder, `PATH`, the profile, the key. With a
   fake set in tests.
2. The archive for `aarch64-apple-darwin`, then Linux arm64.
3. Switch the process-tree runner and the native script modules to it; delete `host_tools`
   and its readers; ban the name.
4. Real runs: tempo's jemalloc, rocksdb, aws-lc; turso's sqlite and mimalloc.
