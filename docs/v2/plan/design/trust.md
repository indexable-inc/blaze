---
status: plan
summary: Secure by default for dependencies. Every crate that is not a workspace member is denied `unsafe` and every ambient capability (fs, net, process, env, clock, thread, global state, randomness, FFI, build script, proc macro) unless the root manifest's `[workspace.metadata.blaze.trust]` grants it by name, with a reason and optionally an exact version. The `blaze::trust` lints read this off the effect walk with one shortest witness each; `blaze audit grants` prints the table a workspace needs today. Built is the lint layer; the driver does not pass the grants yet.
date: 2026-11-13
---
# Trust: dependencies get only what the workspace grants

The owner, 2026-11-13: "assume everything is not secure by default ... by default not allow
any unsafe in any sub crates ... except if opt in". The threat is a compromised or malicious
new version of a dependency. The answer is a deny-by-default capability table in the root
manifest, checked on every build from facts blaze already computes.

## The table

```toml
[workspace.metadata.blaze.trust]
libc = { allow = ["unsafe", "ffi"], reason = "the C library's bindings" }
left-pad = { allow = ["fs"], reason = "reads its config", version = "=1.4.0" }
```

- One entry per package, `-` and `_` read as one. Every malformed entry is an error naming the
  manifest, the entry and what is wrong: an unknown key or capability, no `allow`, an empty
  `reason`, a version without `=`, a toolchain crate, two spellings of one name.
- `version = "=x"` ends the grant at the next version: a new release is granted nothing until a
  person reads it and moves the pin. `blaze audit grants` always pins.
- std, core, alloc, bstd, proc_macro and test are the toolchain's.

## Capabilities and the walk

The capability enum (`crates/blaze-lints/src/blaze/trust/capability.rs`) is the one home of
the mapping from the walk's rules:

| walk rule | capability |
| --- | --- |
| `Io` through `std::fs`, `std::io`, `std::path`, `std::os::*::fs`, `std::os::fd` | `fs` |
| `Io` through `std::net`, `std::os::unix::net` | `net` |
| `Io` through `std::process`, `std::os::unix::process` | `process` |
| `Io` the path does not tell (a body's own, or a call outside those modules) | `fs`, `net`, `process`, said as not split |
| `Env`, `Clock` | `env`, `clock` |
| `Thread` through `std::thread` (a body's own `Thread` too) | `thread` |
| `MutableState` | `global_state` |
| `Random` (not std's `HashMap` keys) | `random` |
| `Foreign`, `ForeignUnknown`, `ForeignUnwind` | `ffi` |
| `Unchecked`, `ForeignUnknown`, `Dyn`, or a bound reached | every runtime capability, said as unseen |
| `Address`, `Unsafe`, `Panic`, `Overflow`, `Alloc`, `AllocIn`, `Block`, `Open`, `Loop`, `Recursion`, `Indirect`, `Precondition`, `Sync`, `Lock` | none |

`unsafe` (blocks, `unsafe fn`/`impl`/`trait`, `asm!`, `extern` blocks, symbol attributes), a
build script, a proc macro and foreign declarations are read off the crate's items wherever
they are, never off the walk. A build script that depends on `cc`, `cmake`, `bindgen`,
`pkg_config` or `vcpkg` is `ffi`.

**Attribution.** A capability is the crate's whose body makes the call into the toolchain, or
does the thing itself (`hir_ty_pure::trust::edge`): what the toolchain does below (its own
`unsafe`, foreign calls, statics) is the toolchain's, and code the toolchain calls back (a
closure, a trait impl) is attributed to its own crate. `trust::attribute` is the pure core,
property-tested against a breadth-first model, with a planted bug (attribution to the
caller's crate) the oracle catches.

**Reached and not.** Uses are the union over the functions the workspace's crates reach from
every one of their functions. A dependency function nothing reaches is read on its own
(`unreached`), so a capability only there is a warning, not a denial: the next call may reach
it.

## The lints

| lint | default | says |
| --- | --- | --- |
| `dependency_capability` | deny | a reached capability not granted; with no table, one finding naming `blaze audit grants` |
| `dependency_unsafe` | deny | `unsafe` anywhere in a dependency, not granted |
| `dependency_capability_unreached` | warn | a capability only in code nothing reaches yet |
| `trust_grant_unused` | warn | a grant for no dependency, a stale pin, or a capability not used |
| `workspace_unsafe` | allow (warn in blaze's ratchet) | a member with `unsafe` and no `#![blaze::trust(unsafe, reason = "..")]` |

The workspace's findings are given once, at its first library crate by name, a library before
a binary. Each names the package, its version, the capability and one shortest chain of calls:
"`main::go` → `left_pad::config` calls `std::fs::read`". Deterministic: sorted by package,
version and capability; the chain is the first shortest one a breadth-first walk from the roots
in crate order meets.

## Cost against the floor

The floor of an edit of one workspace body: rewalk that body, then redo the reachability from
the workspace's roots only through instances whose edges changed, plus reading the grants.
Nothing of a dependency is recomputed.

This design: per-instance edges (`edge`, `toolchain_call`) and per-crate facts (`anywhere`,
`unreached`) are tracked queries, so a dependency's facts are computed once and reused on every
workspace edit. Per workspace crate, `reached` reruns its breadth-first pass over the cached
edges: O(reached instances) hash lookups per edited crate, against the floor's O(changed
instances). For a workspace whose reach is mostly dependencies (tens of thousands of
instances) that is a pass of a few milliseconds, over the 10% bound only when the walk itself
is cheap. The fix, when a profile shows it, is incremental reachability (a per-instance
`reaches` fold), not a cache in front.

## Not built yet

1. The driver: `blaze check`/`blaze build` read the root manifest's table into `Conf::trust`
   (`Grants::of_metadata`), and blaze's own root manifest gets its table first, from `blaze
   audit grants`, or every blaze build fails.
2. `blaze audit grants` (the audit lane): calls `trust::facts::workspace` and prints
   `trust::table::grants(..).toml()`.
3. Merge the older `[workspace.metadata.blaze.trusted]` into `trust`: one concept, one table.
4. A per-capability pass before the answer (a deny lint as a blocking pass, not a check-time
   lint), so a denied dependency stops the build before codegen.
5. Narrower `fs`: per path prefix (`fs = ["$OUT_DIR"]`) once bstd's capabilities name paths.
