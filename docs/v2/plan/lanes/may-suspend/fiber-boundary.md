---
status: current
summary: the blaze lint `blaze::fiber_boundary` (crates/blaze-lints/src/blaze/fiber.rs) rejects a path to `blaze_graph::sched::fiber`, corosensei or the fiber driver written outside `blaze_graph::sched`, so fibers stay the scheduler's alone; blaze's tree is to deny it from the workspace's `[workspace.metadata.blaze.lints]`.
date: 2026-10-31
---
# `blaze::fiber_boundary`: fibers are the scheduler's alone

A fiber switch is only sound where the scheduler knows what is held across it: may-suspend
(`docs/v2/plan/lanes/may-suspend.md`) infers which calls can park from corosensei's
`Yielder::suspend` up through the scheduler's own API. Code that reaches corosensei or the
fiber driver directly makes switches the scheduler never sees, and the audit that may-suspend
replaces has to start again. So only `blaze_graph::sched` and the modules inside it may name
them; everything else goes through `sched`'s API (`sched::wait`, `sched::carry`, ...).

## What it flags

A written path (one whose parent is not itself a path: in a `use` tree, a type, an expression
or a pattern), written outside the module `blaze_graph::sched` and its descendants, when:

1. it, or one of its qualifiers, resolves to the module `blaze_graph::sched::fiber`
   (`use blaze_graph::sched::fiber;`, `fiber::wait()`, `crate::sched::fiber::Carried`);
2. it, or one of its qualifiers, resolves to the crate `corosensei` or any item defined in it
   (`use corosensei::Coroutine;`, a later `Coroutine`); `extern crate corosensei;` too, by the
   dependency it names;
3. it resolves to the fiber driver: an item defined in `blaze_graph::sched::fiber` named
   `Driver`, `Cx`, `Work` or `Config`, judged by where it is defined, so a re-export such as
   `blaze_graph::sched::Driver` is caught.

Each written path gives at most one finding, at its range, checked from its outermost node
inward; the message says what it names ("`blaze_graph::sched::fiber` is named outside
`blaze_graph::sched`", "`corosensei` is named ...", "the fiber driver `Driver` is named ..."),
and the help points to `sched`'s API. There is no exemption comment.

Whether a path is inside `sched` is decided by the module it is written in, never its file: the
lint walks `hir::Module::parent` from that module (a body's owner's module in a body; the item
scope's module elsewhere) to the crate root, and the path is inside when one of those modules
is `sched` directly under the root of the crate `blaze_graph`. A `blaze_graph::util::sched` is
outside.

## How blaze's tree turns it on

It is allow by default and in no group (`blaze::hot` doesn't include it). blaze's own workspace
is to deny it with `fiber_boundary = "deny"` under `[workspace.metadata.blaze.lints]` in the
root `Cargo.toml`, which blaze gives every member as `--deny=blaze::fiber_boundary`; not under
`[workspace.lints.blaze]`, which cargo hands rustc, which stops at the unknown tool `blaze`
(E0602). The root has neither line yet; the name is registered in
`crates/blaze-lint-defs/src/lints.rs`'s `BLAZE` table so `#[deny(blaze::fiber_boundary)]` is
not an unknown lint. Both are coordinator requests from lane may-suspend-lint.

The tests are in `crates/blaze-lints/src/blaze/tests.rs` (`mod fiber_boundary`): an outside
import, a path through `sched::fiber` in expressions, types and patterns, `corosensei` from
another crate and from `blaze_graph` outside `sched`, the re-exported driver, names inside
`sched` and `sched::fiber` (no finding), and `sched`'s own API from outside (no finding).

## Cost

One resolution per written path and each of its qualifiers, only while the lint is on; the
module walk runs only for a path that names something, and the crate names compare interned
symbols with nothing allocated except the finding's message.
