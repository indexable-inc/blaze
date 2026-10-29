---
status: plan
summary: The store is never wholly cold: each record keyed by what its value depends on, so a new pin or a sandbox change reuses what it did not change.
date: 2026-10-29
---
# The store is never wholly cold

2026-10-29. Status: plan; nothing below is built yet but the project name (step 1).

## Why

A warm CI run of main takes about 30 minutes and its build job 15. A cold one takes 97 minutes to
build and hours to test, and it is cold after every bump of `.github/blaze-version`, every change
to the sandbox ([#1505](https://github.com/indexable-inc/blaze/pull/1505)) and every toolchain bump.
Two things make it so.

1. CI names the remote's project by the release that builds blaze
   (`blaze-<os>-<arch>-<job>-<version>`, `.github/actions/blaze`), so a new pin pulls nothing even
   when its keys are the old ones.
2. Every record the store keeps is hashed with one identity per process (`Identity::current` in
   `crates/blaze-store/src/lib.rs`): the store's format, `BLAZE_SOURCES` (a hash of every file of
   every crate of the workspace and the vendored ones, the manifest, the lock file, the cargo
   config and `harness/toolchain`, `crates/blaze-engine/build.rs`) and the toolchain's commit. Any
   change to any of blaze's crates, the sandbox's included, misses every record: compiled
   functions, machine code keyed by its IR, build script runs, test results.

Of the 15 releases from `v0.20261028.7` to `v0.20261029.6`, 5 changed no file of `crates/`,
`vendor/` or `Cargo.lock`, so they keep every key and are cold in CI only for the project's name.
One changed `blaze-cargo` alone. The crates that make code (`hir-ty`, `hir-ty-mir`,
`hir-ty-codegen` and the front end below them) changed in 9.

## The rule

A record's key is a hash of everything its value depends on, and nothing else. What it depends on
comes in two kinds:

- **Inputs**: the source, cfg, features, environment, build script output, target, profile, and the
  layout of the types it uses. These are named in the key itself, by their content.
- **The code that makes the value**: a *stage identity*, a hash of the sources of the crates that
  compute this kind of value, taken from the crate graph (`Cargo.lock`), never from a list kept by
  hand. A stage whose crates did not change between two releases keeps its keys.

A key that misses an input is a correctness bug: each input gets a case in `harness/keys.py` (or a
unit test, where a harness cannot reach it) that changes the input and sees the value rebuilt. A
stage identity that leaves out a crate whose code shapes the value is the same bug: the crate split
is what proves a stage's code is only its crates, and a test checks the identity moves when a file
of a crate of the stage changes and stays when a file outside does.

## Steps

Each step is one pull request, landed and measured before the next.

### 1. The remote's project is named by what keys are made with

`blaze-engine cache build` prints sixteen hex digits of the format and `BLAZE_SOURCES`
([#1437](https://github.com/indexable-inc/blaze/pull/1437)); CI names the project by it in place of
the release ([#1481](https://github.com/indexable-inc/blaze/pull/1481)). A pin bump to a release of
the same sources is then warm. The name only picks which packs are worth pulling, never what a key
finds, so it cannot serve a wrong value.

### 2. A dependency is named by its inputs

In a build with nothing of rustc's, a crate from a registry or git is named in every key it is in
by `blaze crate <name> <place>` (`own_crate_id` in `crates/blaze-engine/src/lib.rs`): its name and
where cargo put its source. Its features, its cfgs, the variables its build script set and the
files the script wrote are not named. Two checkouts sharing one store (the default) that build one
version of a dependency with other features find each other's code. The whole-tree identity does
not cover this; it is a bug today.

The fix names such a crate by the hash `Contents::library` takes, extended with what the checkout's
hash already has (`Contents::hash`): its edition and crate attributes, the environment with its
places written as `$PACKAGE` and `$OUT`, the files its build script wrote, what `include_bytes!`
read, and its dependencies by the same name. `harness/keys.py` gets a dependency fetched from git
with cases for a feature, a cfg its build script sets and a file its build script generates code
from.

This is also what lets step 5 leave the sandbox and cargo's plan out of a function's identity: once
every input they produce is in the key, their code is not.

### 3. Stage identities, from the crate graph

`crates/blaze-engine/build.rs` reads `Cargo.lock` and hashes, for each stage, the closure of the
stage's root crate: the files of each workspace or vendored crate in it, and the name, version,
source and checksum of each crate from crates.io. `Cargo.lock` lists the dependencies of every
target and feature, so a closure can only be larger than what is linked, never smaller. The store
takes one identity per stage (`Store::insert` and `Store::get` name the stage) in place of one per
process. To begin with every stage's root is `blaze-engine`, which is today's identity less the
crates nothing links (benches, the site, the fuzzer), so no key misses more than now.

Test: a unit test of the closure over a small lock file, and one that a change to a crate outside a
stage's closure leaves its identity alone and one inside moves it.

### 4. Machine code by its IR, in a crate of its own

Code made from Cranelift IR (Cranelift's incremental cache, `StoredCode` for LLVM, in
`crates/hir-ty-codegen/backend.rs` and `cached.rs`) is keyed by the IR itself, which is the whole
input of that stage. Its value depends on nothing of the front end, but it lives in
`hir-ty-codegen`, whose closure is the front end. Moving IR to object code (the backends, their
options, what is done to the code after) into a crate `blaze-backend` that depends on Cranelift,
LLVM and `object` and on no `hir-*` crate makes its stage identity that crate's closure. A release
that changes MIR building or type inference then reuses every function whose IR came out the same:
the front end runs again, the code generator does not.

Before it: measure in CI, with `BLAZE_STATS`, what share of the cold build is the code generator,
which bounds what this step buys.

### 5. Build script runs keyed by the sandbox, not by the compiler

A build script's run is keyed by its executable and its environment (`cargo/script.rs`) and hashed
with the whole tree. Its stage is the sandbox and the runner: the crate that holds them (split out of
`blaze-cargo` where needed), plus the executable's hash already in the key. A sandbox change then
reruns every build script and nothing else: a script whose output is the same leaves every key
built on it the same (step 2), and one whose output changed rebuilds what reads it.

### 6. The compiled function's stage

A function's value is shaped by the front end and the code generator, and also by the engine: the
`SymbolResolver` it hands `hir-ty-codegen` names symbols, source paths in panic messages and the
debugger's directories. So the stage is `blaze-engine`'s closure until that code moves into a
crate that depends on neither `blaze-cargo` nor the daemon. Moving it, and the code that turns
cargo's plan into the database's inputs, is the step that takes cargo integration, the daemon, the
CLI and the remote out of the identity of every compiled function.

### 7. Toolchain bumps

The toolchain's commit is in every identity. A bump that leaves a crate of the standard library the
same could keep its code, and code generic over it, once the library's crates are named by their
content as step 2 names dependencies. Left for after 6: it needs the instances of std's generics a
crate uses in its key.

## Measuring

The figure is the time of CI's build job for the first push to main after a pin bump, before and
after. Today it is 97 minutes, against 15 warm. ix VMs cannot be had while this is written, so
each step is measured on its pull request's CI and on the first main run after the next pin bump.
A step takes effect only once a release with it is the pin: CI builds blaze with the pinned release,
whose keys are what the store holds.
