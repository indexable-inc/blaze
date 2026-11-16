---
status: plan
summary: hvbox, an experimental sandbox that runs a macOS build step's Mach-O as a Hypervisor.framework guest with no guest kernel, every system call and fault answered by a host-side Sentry that names each refusal and records every read, miss and listing.
date: 2026-11-16
---
# hvbox: a kernel-less sandbox for macOS build steps

Experimental, in its own workspace (`crates/blaze-hvbox`, excluded from the root). Nothing in
blaze depends on it or selects it. Leads marked UNVERIFIED are from memory, to be checked by a
test before anything rests on them.

## Why

blaze's macOS path for native build scripts (`blaze-proc-macro/src/native_sandbox.rs`,
`sandbox-exec` with a deny-default profile) gets a bare `EPERM` from the kernel and records no
reads. A layer that every call passes through can name each refused path and its rule, and
record every read, miss, listing and environment read, which is what a step's result is keyed
by. It is gVisor's shape on the Mac: no Seatbelt, no root, no macOS VM.

## What it is for (decision D92)

The owner, 2026-11-16: a build script itself runs inside blaze's own execution (a wasm module
in most classes, docs/v2/plan/design/build-scripts.md); hvbox is the executor for what a script
**spawns**: a C compiler, a linker, any command, native code, picked by the target
architecture, under the determinism contract below: the same outputs for anyone on any host.
The seam is a spawn request from a build script: program, arguments, the whole environment, the
directory, the grant of readable and writable paths, the step's key (seeding the clock and
randomness). That is blaze's existing `blaze_core::Command` with its `Sandbox`; the helper's
input is that value, so wiring hvbox in later is a strategy choice at one seam, not a second
interface.

## Layering

1. **Guest**: the step's own Mach-O, the host's real `/usr/lib/dyld` and the dyld shared cache,
   running at EL0 in a Hypervisor.framework VM. No guest kernel.
2. **Stub**: one EL1 page: vectors whose every entry is `hvc #n; eret`, so an EL0 `svc`, a
   fault, a trapped counter read or an undefined instruction leaves to the host. The stub holds
   no state and makes no decision.
3. **Core** (`blaze-hvbox-core`, `no_std`, no `alloc`, sans-IO): a state machine. In: an
   `Exit` (the guest's registers and syndrome, from any driver). Out: an `Effect` (a host-neutral
   primitive). Owns the call tables, the verdicts, the path view, the guest address-space
   bookkeeping, the Mach-O reader, the shared-cache reader, the trace, time and randomness.
4. **Driver** (`blaze-hvbox-host`): the only code that touches Hypervisor.framework and the real
   kernel: it maps memory, runs the vCPU, performs effects, feeds results back.

## Exit protocol

The seam is one type, `core::exit::Exit { kind, regs: [u64; 31], pc, sp, pstate, syndrome,
far }`, with `kind` one of `Svc`, `Abort { stage2 }`, `Counter`, `Undefined`, `Other(esr)`.
Hypervisor.framework fills it from `ESR_EL1/ELR_EL1/FAR_EL1/SPSR_EL1` after the stub's `hvc`, or
from the exit's own syndrome for a stage-2 fault. A KVM driver or a seccomp-trap driver fills
the same struct; nothing in the core names Hypervisor.framework. The core answers with a
`Step`: zero or more `Effect`s (`OpenHost { path, flags }`, `ReadHost`, `MapGuest { va, len,
prot, backing }`, `Forward { call }` on a same-ABI host, `Answer { x0, x1, carry }`,
`SetThreadPointer`, `Exit(status)`, `Stop(Refusal)`), each completed by the driver with a
result the core consumes. Darwin's ABI (UNVERIFIED, checked by the hello run): number in x16
(positive BSD, negative Mach trap, `0x80000000` the machine-dependent class that sets the
thread pointer), arguments x0..x7, error as carry set and errno in x0.

## Verdicts

Every call number has one row: `Forward` (passed to a same-ABI kernel after its arguments are
translated and checked: `write` to an inherited descriptor, `fstat`, `close`, `read`),
`Rewrite` (forwarded after its path is resolved inside the view: `open`, `stat`, `access`,
`getattrlist`), `Emulate` (the Sentry answers itself: `mmap`/`munmap`/`mprotect` and the Mach VM
traps, `thread_set_tsd_base`, `getpid`, `task_self_trap`, `bsdthread_*`, `getentropy`,
`shared_region_check_np`, `issetugid`, AMFI's `__mac_syscall`) or `Refuse` (an errno to the
guest and a trace line with the number and arguments: sockets, `kill`, `ptrace`,
`task_for_pid`, `mount`, `reboot`, `sysctl` writes, `csops` writes, `fork`/`posix_spawn` until
the spawn design below is built). The table starts empty; a call is added when a trace shows the
startup path needs it. Each row also says `ForwardableSameAbi` or `EmulatedAlways`, so the list a
Linux host must emulate is the table, not a discovery.

## File view

A `Grant` is a read set and a write set of absolute prefixes. A path is resolved by the Sentry
one component at a time (lexical `..`, each symlink read and its target resolved again, a work
list, a hop limit), every intermediate and final path checked against the grant; the final open
then passes `O_NOFOLLOW_ANY` so a symlink raced in afterwards fails in the kernel. A refusal
names the path and the rule: `refused: /Users/x/.ssh/id (not under the read set)`. The write set
is one scratch directory; no create, write, unlink, rename, chmod or truncate elsewhere. Later:
a chroot-like rewrite (`/out` to the step's `OUT_DIR`), case-insensitive lookup kept as the host
file system does it.

## Network

Refused, every family, every call, with no opt-in in this crate.

## Tracing

Every resolved open is a trace row: `read path`, `miss path` (ENOENT inside the view),
`list path` (a directory read by `getdirentries64`/`getattrlistbulk`), `write path`; refusals are
rows too. Rows reach output sorted and deduplicated, so a run's trace is deterministic. This is
what blaze keys the step by (`blaze_core::Traced`).

## Environment reads by name

After `exec` the environment is plain memory; `getenv` never crosses the boundary. Two choices:
lend variables the way `blaze-usercode/src/confine.rs` does (`GIVEN` and the walk set: the
program's environment holds only the given variables, every other one is read by name from
outside, traced), or interpose `getenv` in the guest. **Decision: lend.** The guest's initial
stack carries only the given names; a lent variable is reached through an interposed `getenv`
that is a `svc` to an hvbox-private call the Sentry answers and traces, installed as an ordinary
dyld interposing image the Sentry maps. The interposer is the mechanism, lending is the policy:
interposing alone would still put every variable in memory where a walk (`environ`) reads it
untraced; lending makes the walk see only the walk set, as confine.rs's design does.

## Time and randomness

`CNTKCTL_EL1` denies EL0 counter and frequency reads, so every `CNTVCT`/`CNTFRQ` read traps to
the Sentry. The Sentry builds the commpage itself (a copy of the host's with the user timebase
marked unusable, UNVERIFIED offset) so libSystem takes the `mach_absolute_time` trap. The clock
the guest sees is virtual (next section). `getentropy` and `/dev/random`, `/dev/urandom` reach
the Sentry, which answers from a generator seeded by the step's key, so two runs of one key see
the same bytes.

## Personalities and determinism

**Two personalities over one model.** A personality only decodes and encodes one system-call
ABI: XNU (built first) and Linux aarch64 (later). Beneath both sits one model in the core: the
file view, descriptors, the process tree, the clock, randomness, the read trace. Determinism and
tracing live in the model, so no personality reimplements them. In code: `core::personality`
holds the seam (`xnu` now), the rest of the core is the model.

**The contract.** Everything the guest can observe is a function of the step's key and its
declared inputs, never of the host:

| value | where it comes from |
|---|---|
| clock (`mach_absolute_time`, `CNTVCT`, `gettimeofday`, `clock_gettime`, file times it sets) | a virtual clock: starts at a fixed epoch, advances by a fixed amount per read |
| random bytes (`getentropy`, `/dev/*random`, `RNDR`) | a generator seeded by the step's key |
| pids, tids, port names | counters from fixed starts, in spawn order |
| mapping addresses | fixed placement (no slide; first fit from fixed bases) |
| inode numbers, device numbers | a per-step table assigning numbers in first-seen order |
| directory order | sorted by name |
| file timestamps of inputs | a fixed time (inputs are keyed by content, not by time) |
| CPU count and features, page size, memory size | a declared machine description, the commpage built from it |
| hostname, uname, user and group | fixed values |

A clock read under a virtual clock is no longer an ambient input, so it need not make a result
non-reusable; that decision is the coordinator's and the owner's at wiring time.

**Threads, an open decision for the owner:** per-task clocks and random streams derived from
spawn lineage (keeps parallelism, independent of sibling order, does not fix races inside the
guest), or serialised scheduling (fixes those, costs parallelism). Not chosen here.

**The test** is a property test on blaze's own engine: one step run several times under
perturbed host conditions (real time, load, host pids, host directory order, CPU count) gives
byte-identical outputs and an identical guest-visible trace, and a planted leak (one host value
let through) fails it. Which driver can promise what: a hypervisor driver traps or hides direct
counter reads and the hardware random instruction beneath the EL1 stub, so it can hold the whole
contract; a seccomp-trap driver cannot see a counter read or `RNDR` and holds it only for calls.
Linux serves time through the vDSO, so the Linux personality maps its own vDSO or none.

## Address space, shared cache, pointer authentication

IPA equals guest VA; the stage-1 tables identity-map 1 TB in 32 MB blocks, 16K granule; the
real permissions are stage 2 (`hv_vm_map` flags), which the Sentry owns. The Sentry maps the
shared cache's files itself, applies their slide info as plain pointers, and answers
`shared_region_check_np` with its base: dyld then uses it (UNVERIFIED). Pointer authentication
keys are disabled in `SCTLR_EL1`, so signing and authenticating are identity and plain pointers
work (UNVERIFIED for every instruction class the cache uses). The decoder is checked against the
host's own live shared region: same slide, same non-auth words, same auth words once stripped.
Later: the fixed-up cache pages become a record mapped copy-on-write per process.

## Threads, spawn and fork

One guest thread is one vCPU on its own host thread (`hv_vcpu_create` is per thread). One VM per
host process, so a guest `posix_spawn` or `fork`+`exec` becomes a new helper process given the
child's program, arguments, the inherited descriptors and the same grant, its trace merged into
the parent's. `fork` without `exec` is refused by name until a tool is found that needs it.

## Floor and how the ratio is measured

The floor is native: the same step outside any sandbox. Cost per call is one VM exit (UNVERIFIED,
about a microsecond) plus the forwarded call. Measured with `/usr/bin/time -l` beside the native
run: wall time, instructions retired, peak footprint, for hello, `clang -c`, a build script.
Target: within 10% of native for `clang -c`; the ratio is reported per rung.

## No fallback, one seam

A step hvbox cannot run stops with a typed error naming the step and the reason (the call it
refused, the fault address and syndrome). Choosing hvbox over `sandbox-exec` is a setting at one
seam, a variant of `blaze_core::Sandbox`, never automatic, never a retry with the other.

## What blocks adoption

The `com.apple.security.hypervisor` entitlement on the helper (ad hoc signing suffices locally,
UNVERIFIED for distribution); CI Macs that are themselves VMs without nested virtualization;
the per-release kernel interface: a version table keyed by the host's build, and an unknown
release refused by name; the shared cache's format version, refused by name when unknown.

## Linux hosts (design goal, nothing built)

hvbox should also run on Linux aarch64, so a Linux-only builder runs a macOS build step inside
the layer. The structure allows it: effects are host-neutral primitives; the exit source is the
one `Exit` type (KVM where `/dev/kvm` exists, a seccomp-trap driver for builders that are VMs);
each call row says forwardable on a same-ABI host or emulated. Translation is then the core's
job: errno numbers, `stat`/`dirent` layouts, open and mmap flags, signal numbers, and the file
system behaviours macOS tools rely on (case-insensitive lookup, `getattrlist`, extended
attributes, `clonefile`). The differential test grows a third leg: the same step under the
real macOS kernel, under hvbox on a Mac, and under hvbox on Linux; the Mac certifies Linux.
Refused by name (UNVERIFIED leads): a host CPU without the instruction baseline macOS arm64
binaries assume (and pointer authentication for arm64e tools), and a 64K-page host.

**Open decision for the owner:** on Linux there is no Apple userland to borrow; the guest needs
dyld, libSystem and an SDK, and Apple licenses its binaries for Apple-branded hardware. Either
Apple's binaries only where the licence allows, or a userland built from Apple's open-source
releases. Not chosen here.

## Acceptance ladder

0. This spec, and a freestanding static Linux aarch64 `hello` (`write`, `exit_group` through
   `svc #0`) run to exit under the Hypervisor.framework driver: the trap loop, the tables and
   the exit protocol with no dyld, cache or pointer authentication in the way.
1. A C `hello` runs to exit in the guest; its stdout and status equal the native run's.
2. `clang -c` on one file; the object is byte-equal to the native run's.
3. One small C-building Cargo build script from the reference project.

Throughout, the real kernel is the oracle (same step with and without hvbox, identical
outputs), and a hostile suite must fail inside the guest, each probing only targets under the
scratch directory: a raw `svc` for a call not on the list, a symlink escaping the view, a Mach
service lookup, an open outside the read set, a write outside the write set.
