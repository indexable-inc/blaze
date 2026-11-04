---
status: plan
summary: A blaze edition where std has no ambient authority, main receives an Io capability, code is safe Rust by default, and a function's signature is the complete list of what it can touch.
date: 2026-11-02
---
# Capabilities: an edition where signatures are permissions

In ordinary Rust, any function can open a file, read an env var, dial a host or read the clock,
and nothing in its signature says so. This plan adds a blaze edition where a function can only
touch what it's handed. The signature is the permission list, and a reviewer sees every boundary
by reading types.

```rust
fn main(io: Io) -> Result<(), Error> {
    let Io { fs, env, net, clock, rng, .. } = io;

    let data: Dir<ReadWrite> = fs.open_dir("data")?;
    let cfg: Dir<ReadOnly> = data.read_only();          // narrow

    let stripe = StripeKey::from_env(&env)?;            // reads one named var, nothing else
    let api = net.connect_only("api.stripe.com:443")?;  // Net narrowed to one host
    charge(&stripe, &api, &cfg)
}

// charge can't open files outside cfg, write anything, read other env vars or dial other hosts
fn charge(key: &StripeKey, api: &Conn, cfg: &Dir<ReadOnly>) -> Result<(), Error> { ... }
```

Nothing here is built. It's an opt-in extension (`superset-of-rust`): a crate on an ordinary Rust
edition builds exactly as before.

## The edition

```toml
[package]
edition = "blaze-2026.11"
```

- **Named after the blaze release that introduced it, and cut rarely.** blaze releases several
  times a day (`v0.20261030.0` to `.2`), and every blaze compiles every edition it has shipped. A
  new edition appears only when a release changes semantics: a new std shape, a new check, a newer
  Rust syntax year. Most releases add none.
- **Each blaze edition fixes its Rust syntax year.** `blaze-2026.11` is Rust 2024 syntax plus
  everything below. When Rust 2027 lands, a later blaze edition takes it. Plain `edition = "2027"`
  still means ordinary Rust.
- **Separate from the toolchain pin.** A project pins which blaze builds it
  ([docs/toolchain.md](../toolchain.md)). The edition says what a crate's code means. A
  project can pin blaze `v0.20270301.4` and keep crates on `blaze-2026.11`. Merging the two would
  make every compiler upgrade a semantics change, which is what editions exist to prevent.
- **Migration:** `blaze fix --edition blaze-2026.11` rewrites what it can (ambient calls into
  capability parameters threaded from `main`) and lists the rest.
- **No crates.io.** cargo and rustc reject the edition, which is correct for code only blaze
  builds. Packages live in blaze's own registry, with ports where needed (see below).

What the edition turns on:

1. The edition's own `std`, with no ambient authority.
2. `fn main(io: Io)`.
3. Safe Rust only, checked transitively.
4. No ambient IO, checked transitively, for every dependency including ordinary-edition ones.
5. C dependencies run as wasm.
6. The OS sandbox derived from the capabilities `main` hands out.

## The edition's std

In this edition `std` is a different library under the same name. `use std::fs::Dir` works, and
`std::fs::File::open` doesn't exist. The edition picks which `std` a crate links, the same way an
edition already picks the prelude.

- **Kept:** `core` and `alloc` (`Vec`, `String`, `Box`, iterators, formatting), collections, sync
  primitives, and everything that doesn't reach the outside world.
- **Removed:** every way to reach the world without a capability. That's `std::fs::File::open`,
  `std::env::var`, `std::net::TcpStream::connect`, `Instant::now`, `SystemTime::now`,
  `thread::spawn`, `process::Command` and `std::io::stdin`.
- **Added:** `Io`, `Fs`, `Dir<P>`, `File<P>`, `Env`, `Net`, `Conn`, `Clock`, `Rng`, `Spawn`,
  `Process`, and narrowing methods on each.
- **Implemented as trusted code** on the real std and syscalls, `unsafe` allowed. It's on the
  trusted list (below), and it's the main place to audit.

What stays ambient, on purpose: allocation, panics and stderr. Making stderr a capability means
threading a logger through every function, and people would route around that with a global.
That's worse than an honest exception.

### Capability types

- **Unforgeable.** Each type has private fields. Only `main`'s `Io`, or narrowing an existing
  capability, creates one. `Io` itself is a zero-sized type with a private field.
- **Handles, not markers.** A `Dir` carries a directory fd and a `Conn` a socket, so these aren't
  zero-sized. Permissions are: `Dir<P>` takes a marker type (`ReadOnly`, `ReadWrite`,
  `ReadWriteExec`), and methods are gated by traits (`P: CanRead`). Rust has no union types, so
  `Dir<Read | Write>` is spelled with marker types for now. Const bitflags (`Dir<{R | W}>`) are a
  later extension.
- **Narrowing never widens.** `read_only()`, `subdir("x")`, `connect_only(host)`,
  `env.only(&["STRIPE_KEY"])` each return a capability with at most the parent's authority. No
  method goes the other way.
- **Narrowing happens in the OS too.** `read_only()` reopens the handle with fewer rights
  (`openat2` with `RESOLVE_BENEATH` and `O_RDONLY`), so a trusted `unsafe` block or a child process
  can't write through it.

### Secrets

```rust
#[derive(std::Secret)]
struct StripeKey(SecretBytes);

impl StripeKey {
    fn from_env(env: &Env) -> Result<Self, MissingVar> { env.secret("STRIPE_KEY").map(Self) }
}
```

A `Secret` type has no `Debug`, `Display`, `Serialize` or `Clone`, zeroizes on drop, and gives
its bytes only to functions in its own module, typically a client that also holds a `Net`
narrowed to the provider's host. Leaking the key into a log means writing code in that module that
does it, and review can see it.

## Signatures are the source of truth

In this edition a function can touch only the capabilities in its parameters, or ones it reaches
through them. Ambient IO and untrusted `unsafe` don't compile. So:

- The signature is the complete effect list. Generics carry capabilities through bounds
  (`fn run<F: Fs>(fs: &F)`). A `dyn` trait carries them in its methods' parameters.
- Nothing is published or stored per crate. A crate-level view, `blaze explain caps <crate>`,
  is the union of what its public functions take, derived from signatures.
- `blaze explain caps StripeKey` follows a value through the call graph and lists every function
  it reaches. That's the answer to "who can see the Stripe key".

## Safe Rust only, transitively

`#![forbid(unsafe_code)]` covers one crate. The edition checks the whole program: every `unsafe`
block a binary reaches has to sit in a trusted crate. Otherwise the build refuses it, naming the
function, the `unsafe` block and the call path that reached it.

The check is another effect in the existing walk (`crates/hir-ty-pure`, one memoized summary per
monomorphized instance, the same walk that proves `#[blaze::pure]`, `nopanic`, `noalloc` and
`nonblocking`; see [effects.md](../v2/plan/lanes/effects.md)). Two effects are new:

- **`unsafe`**: reaches an `unsafe` block outside the trusted list.
- **`ambient`**: reaches a syscall or ambient std API (file, env, net, clock, random, process,
  thread) outside the trusted list.

### The trusted list

The workspace names its trusted crates, each with an owner and a reason:

```toml
[blaze.trusted]
std = "the edition's std"
uring-rt = { owner = "runtime lane", reason = "io_uring driver; exposes only capability-taking fns" }
```

`core`, `alloc` and the edition's `std` are trusted by default. Everything else is a reviewed
entry. `blaze explain trusted <binary>` prints the full list a binary depends on. That list is the
security boundary, so it should stay short.

### Ordinary-edition dependencies

A capability crate can depend on an ordinary-edition crate (serde, regex, itertools) if the walk
proves every function the program reaches from it is free of `ambient` and `unsafe` effects. blaze
proves this from source at build time, and trusts no published claim. Most pure libraries pass
unchanged. A function that fails is refused with its path and the effect.

## C as wasm

Native C in the same process can ignore every capability. So capability programs compile C
dependencies to `wasm32-wasip1` with clang and run them under Wasmtime. The C code's imports are
exactly the capabilities the Rust caller hands it. blaze already runs proc macros this way (rule
`proc-macros-on-wasm`), so the runtime exists.

- Expect roughly 1.5 to 2x slower C-heavy code. A measured number goes here when phase 5 runs.
- A native C escape exists only as a trusted-list entry with a reason, and the OS sandbox still
  holds it to `main`'s capabilities.

## The OS sandbox follows the signature

The capabilities `main` hands out become the process's sandbox policy:

- **Linux:** Landlock rules from the `Dir`s and their permissions, a network policy from the
  narrowed `Net`s, `openat2(RESOLVE_BENEATH)` for every path.
- **macOS:** a sandbox profile generated the same way.

blaze already sandboxes build scripts and proc macros (rule `sandbox-everything`). This reuses
that machinery for user programs. Trusted `unsafe` code or a C escape is still held to what the
signature allows. When a platform can't build the policy, the program stops with an error naming
the capability it couldn't enforce. It never runs unsandboxed (`no-fallback`).

## Sans-IO and io_uring

- **A sans-IO core takes no capability.** With `ambient` checked, a function whose signature has
  no capability provably does no IO. Today's lint against `Instant::now`
  (`sans-io-cores`) becomes a type fact: `Clock` is a capability, so a core without one can't read
  time.
- **Tests pass a simulated `Io`:** an in-memory `Fs`, a logical `Clock`, a seeded `Rng`, a
  scripted `Net`. Replays are deterministic, with no global mocking.
- **io_uring:** `Dir` is a dirfd in the ring's fixed-file table, `open` is `IORING_OP_OPENAT2`
  with `RESOLVE_BENEATH`, and the ring's file table is the capability table. A driver holds the
  `Io` pieces and feeds the core, the shape `sans-io-cores` already asks for.
- **Async:** the runtime is a trusted crate on io_uring whose spawn takes `Spawn` and whose IO
  takes the capability it acts on.

## Registry and ports

blaze's own registry hosts `blaze-2026.11` packages. It stores nothing beyond source and
metadata: signatures carry the effects, and blaze proves ordinary-edition code at build time.

Ports needed first, because everything networked depends on them:

1. An async runtime on io_uring that takes `Io` (replaces tokio).
2. An HTTP client that takes `Net` (replaces reqwest), and a server that takes a bound listener.
3. TLS that takes `Rng` and a `Dir<ReadOnly>` for roots, or a pure Rust TLS over a `Conn`.
4. Process, terminal and env helpers over `Process` and `Env`.

Pure crates (serde, regex, itertools) need only mirroring.

## Phases

1. `unsafe` and `ambient` effects in the existing walk, plus `blaze explain trusted`. Useful on
   ordinary Rust right away: report what a binary can reach.
2. The edition: `edition = "blaze-2026.11"` in manifests, the edition's `std`, `fn main(io: Io)`,
   capability types with narrowing, and the transitive safe-only and no-ambient checks.
3. Secrets (`#[derive(std::Secret)]`) and `blaze explain caps`.
4. OS sandbox from `main`'s capabilities, on Linux (Landlock, `openat2`) then macOS.
5. C dependencies as wasm, with a measured cost.
6. The io_uring runtime, HTTP and TLS ports, and the registry.
7. `blaze fix --edition` migration from ordinary Rust.

## Open questions

- Should `Io` destructure (`let Io { fs, env, .. } = io`), or hand out pieces by method, so `main`
  can't keep authority it didn't mean to?
- Thread-local and global state. `static` with interior mutability can smuggle a capability across
  calls. Should the edition forbid storing capability types in statics (a `!Static` auto trait)?
- Revocation. A capability handed to a task can't be taken back today. Should there be revocable
  wrappers (`Revocable<Dir<P>>`), as in E and Fuchsia?
- How does the GPU device API ([gpu/plan.md](gpu/plan.md)) fit? `Device` is probably a
  capability from `Io`, so kernels can't reach hardware they weren't handed.

## Prior art

- cap-std (Bytecode Alliance): `Dir` and friends with no ambient authority, in Rust.
- WASI preopens: the same model, enforced by the runtime.
- Zig's `std.Io`: an IO interface passed explicitly and swappable between thread pool, event loop
  and test.
- Austral: linear capabilities in the language.
- Pony: reference capabilities.
- E and Mark Miller's object-capability work: narrowing, revocation, confused-deputy analysis.
- Fuchsia: handles with rights.
- Capsicum and Landlock: OS-level capability modes.
- Effekt and Koka: the same idea as effect handlers.
