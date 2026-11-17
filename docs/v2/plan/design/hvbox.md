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

## Incident 2026-11-16

**What was run.** At 15:37:51 the lane building hvbox rebuilt the helper, signed it ad hoc with
the hypervisor entitlement and ran it on a C `hello` (`timeout 30 blaze-hvbox run --verbose --
hello`) on the owner's Mac. That run mapped every mapping of the dyld shared cache into the
guest with `hv_vm_map` (0x1a2f8c000 bytes), took five exits, refused `thread_selfid` (372) and
stopped with a typed error at a stage-2 data abort at address 8. Nothing went wrong on the host.
At 15:37:59 and 15:38:03 the lane added an answer for `thread_selfid` and ran the helper again.

**What happened.** At 15:38:05 the kernel panicked and the Mac rebooted, with every other lane's
work cut off mid-step: `[SPTM] VIOLATION_ILLEGAL_MAPPING_TYPE: sptm_map_page(sptm.c:583) -
root_pt_fte->type(XNU_STAGE2_ROOT_TABLE), leaf_pt_fte->type(XNU_STAGE2_PAGE_TABLE),
page_fte->type(XNU_USER_EXEC), vaddr(0x180000000)`, panicked task pid 24783, `blaze-hvbox`. The
page-table monitor refused to put a page typed as user-executable into a guest's stage-2 table,
at guest address 0x180000000, and its refusal is a panic of the whole machine. The second run's
own output was lost in the reboot.

**The cause, inferred, not reproduced (and never to be reproduced).** The helper backed the
shared cache with private file mappings (`mmap(PROT_READ | PROT_WRITE, MAP_PRIVATE, fd,
offset)`) and handed them to `hv_vm_map`. A page of a private file mapping that nobody has
written is the file's own page frame. 0x180000000 is the cache's base and its first mapping:
the main cache file from offset 0, 0xb4000 bytes, read and execute, with no slide info, so the
helper never wrote it; its frames are the cache text the system maps executable into every
process, hence `XNU_USER_EXEC`. The first run survived although it mapped the same ranges, so
`hv_vm_map` only records the mapping and the kernel enters each page in the stage-2 table when
the guest first touches it; the first run stopped before dyld touched the cache. With
`thread_selfid` answered, the second run most likely went on to `shared_region_check_np` and
dyld's first read of the cache header at 0x180000000, which made the kernel enter that frame.
Confidence: high that the page was the first mapping of the main cache file (the panic names
exactly its address, and it is the only file-backed executable mapping there); medium that the
trigger was dyld's first read (it fits the timeline and the first run's survival, but the
second run's log is gone).

**What was not known beforehand.** That this machine's page-table monitor types every
physical page and answers a typed page in a stage-2 table with a machine panic rather than an
error from the call; that `hv_vm_map` accepts such memory without complaint; that a mistake in
an unprivileged helper's memory could take the owner's Mac down at all. The first brief's rails
assumed the worst a guest or the helper could do was hang the helper.

**The rule that follows.** Only anonymous private memory the helper allocated itself ever
reaches the stage-2 table, by construction (Host safety, below); a file's bytes reach the guest
only by being copied in. No process calls Hypervisor.framework until the owner allows it by
hand (the consent file, below).

## Host safety

The rule: **the only memory that can reach `hv_vm_map` is a `GuestMem`**
(`host/src/guest.rs`): fresh anonymous private memory (`bstd::mem::Region::reserve_sparse`:
`MAP_PRIVATE | MAP_ANON | MAP_NORESERVE`, `PROT_READ | PROT_WRITE`), never executable on the
host, never `MAP_JIT`, never a file, never shared, never the host's shared region or commpage.
Its fields are private and its one constructor is that reservation. The framework's functions
are declared only inside `host/src/vm.rs` (a private module), `hv_vm_map` is called only by
`Vm::map`, and `Vm::map`'s only memory argument is a range of a `GuestMem` the `Vm` owns. The
helper makes no file mapping at all: program, dyld and cache are read with bstd's
`File::read_into`, a guest's file `mmap` is `Backing::Copy` (a `pread` into fresh guest
memory). Every touch of the kernel through the framework:

| call | what could go wrong on the host | the rule that prevents it |
|---|---|---|
| `hv_vm_config_*`, `hv_vm_create` | a VM made by a process nobody allowed | `Vm::new` takes a `Consent`, made only by reading the owner's consent file |
| `hv_vm_map` | a typed page (file-backed, executable, JIT, shared, kernel) entered in stage 2: the incident's panic | only a range of a `GuestMem` the `Vm` owns; nothing else has the signature |
| `hv_vm_map` | host memory freed or reused while the guest still maps it | the `Vm` owns each `GuestMem`; it is released only after its last stage-2 range is unmapped, or after `hv_vm_destroy` |
| `hv_vm_protect` | stage-2 change of a range the helper did not map | only ranges the core's space holds; the cache, mapped in chunks, may not be reprotected (`Stop::CacheChange`) |
| `hv_vm_unmap` | unmapping a range no slot backs, freeing the wrong memory | `Vm::unmap` finds the slot backing the host range first, or fails typed |
| `hv_vm_destroy` | destroying with a vCPU alive | the `Vcpu` is declared after the `Vm` and drops first |
| `hv_vcpu_create`, `_destroy` | a vCPU without a VM, or on another thread | `Vcpu::new` takes the `Vm`; one vCPU, made and run on the calling thread |
| `hv_vcpu_run` | guest code reaching the host | none can: the guest runs at EL0 under the stub at EL1 inside the VM; every exception is an exit; the loop is bounded (`MAX_EXITS`), the run under `timeout` |
| register access (`hv_vcpu_get/set_reg`, `_sys_reg`) | a bad value harming the host | they set only the guest's registers; a wrong one faults the guest into an exit |
| exits | acting on a guest-chosen host address | the core translates guest addresses through its space; the driver checks every host address the core hands it (memory reads and writes, every pointer of a forwarded call) lies inside a `GuestMem` (`Vm::holds_host`) and stops typed otherwise (`Error::OutsideGuest`) |
| forwarded calls (`svc`) | the guest acting on the host as the owner | the call table: only listed calls, pointers translated and checked, paths through the grant, network, `kill`, `ptrace`, task ports, `mount`, `reboot`, spawning refused by name |
| the commpage | the host's page entered in stage 2 | its bytes are read and copied into a `GuestMem`; the host's page is never mapped |

UNVERIFIED, a residual risk to watch in the first runs: what frame an anonymous page the host
never wrote gets when the guest touches it first. XNU zero-fills a fresh page for anonymous
memory as far as known; a shared zero page, if one exists, would be typed as data, not
executable. The predecessor's Linux `hello` (15:36, from its scratch output) ran to exit on
anonymous memory mapped executable at stage 2 with nothing amiss on the host.

## First run checklist

For when the owner allows a run; nothing below happens before he says so.

1. The owner saves his work and closes what matters: a mistake here can panic and reboot the Mac.
   No other heavy job runs.
2. The agent builds (`cd crates/blaze-hvbox && CARGO_TARGET_DIR=<notes>/target timeout 300 nice
   -n 10 cargo +nightly-2026-10-27 build --offline -j 4`) and the core's tests pass.
3. The owner creates, by hand, `/Volumes/Projects/blaze-int-1112-notes/port27/hvbox-1116/OWNER-ALLOWS-HYPERVISOR-RUN`
   holding exactly the line `I, the owner of this Mac, allow blaze-hvbox to call
   Hypervisor.framework.` Without it the helper exits 121 before any framework call.
4. The agent signs the helper ad hoc with `host/hv.entitlements.plist` and runs, once, the
   freestanding Linux `hello` (no dyld, no cache): `timeout 30 <helper> run --verbose
   --resident-cap 268435456 -- <notes>/bin/linux-hello`. Expected: `hello from a linux guest`,
   status 7.
5. Only if that was clean: once, the C `hello` with `--resident-cap 1073741824`. Each run is
   one command, read before the next; no loop of reruns, no edit between a run and reading it.
   Stop at the first of: a kernel message or panic, a hang past `timeout`, any `HvError`, any
   result not understood. Afterwards the owner deletes the consent file until the next session.

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
   kernel: it maps memory, runs the vCPU, performs effects, feeds results back. It uses
   blaze's own standard library, bstd (`crates/blaze-std`, a path dependency), wherever bstd
   has what it needs: its entry, arguments and standard error; file reads through a read-only
   world rooted at the host's `/`; guest memory from `bstd::mem::Region::reserve_sparse`; its
   tables in the run context's frame. What bstd lacks is worked around beside its one use and
   listed in the crate's DEBT block: a positional read of a file above a command's start
   folder (`Ancestor::open_read`), `File::read_exact_at`, byte (non-UTF-8) arguments, and a
   read on a raw descriptor (the guest's own descriptors, read with the forwarding `svc`).
   This brings blaze-core and its third-party crates (libc) into the crate's lock file, which
   the first brief's "no third-party crates" did not foresee; the owner's "use bstd" decides.

## Exit protocol

The seam is one type, `core::exit::Exit { kind, regs: [u64; 31], pc, sp, pstate, syndrome,
far }`, with `kind` one of `Svc`, `Abort { stage2 }`, `Counter`, `Undefined`, `Other(esr)`.
Hypervisor.framework fills it from `ESR_EL1/ELR_EL1/FAR_EL1/SPSR_EL1` after the stub's `hvc`, or
from the exit's own syndrome for a stage-2 fault. A KVM driver or a seccomp-trap driver fills
the same struct; nothing in the core names Hypervisor.framework. The core answers with a
`Effect` (`Resume` with the registers to set, `Host(call)` on a same-ABI host carrying the
span of every pointer argument, `Map { va, len, prot, backing }` with `backing` either zeros
or `Copy { fd, offset }`, `Fill` of one chunk of a region filled on demand, `Unmap`, `Protect`,
`Exit(status)`, `Stop(reason)`), each completed by the driver with an `Outcome` the core
consumes, which may yield the next effect. A call whose memory argument lies in an unfilled
chunk is answered with the `Fill` first and then answered again. Darwin's ABI (UNVERIFIED, checked by the hello run): number in x16
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
let through) fails it. Its first form, at the core's seam with no hypervisor, exists
(`core/src/tests.rs`, `blaze_sim::check`): a scripted guest (entropy, clock reads, an
anonymous map, thread id, pid) under perturbed host addresses, host memory layout and host
descriptor numbers sees identical answers and identical memory, and the planted control (a
forwarded call answered differently by two hosts) is caught. That control is also a finding:
a forwarded call's result is the host's (an `fstat64` carries the host's inode, size and
times), so today the contract holds for emulated calls only; forwarded results must pass
through the model (the inode table, fixed input times) before the contract holds whole. The
whole-step form needs a run and waits for one. Which driver can promise what: a hypervisor driver traps or hides direct
counter reads and the hardware random instruction beneath the EL1 stub, so it can hold the whole
contract; a seccomp-trap driver cannot see a counter read or `RNDR` and holds it only for calls.
Linux serves time through the vDSO, so the Linux personality maps its own vDSO or none.

## Address space, shared cache, pointer authentication

IPA equals guest VA; the stage-1 tables identity-map 1 TB in 32 MB blocks, 16K granule; the
real permissions are stage 2 (`hv_vm_map` flags), which the Sentry owns. Every guest page is
host memory the helper allocated itself (Host safety, above). The shared cache (about 7 GB of
mappings on the host of 2026-11-16) is filled on demand: each of its mappings is a lazy region
of reserved anonymous memory, nothing mapped; a stage-2 fault in an unfilled 256 KB chunk
copies that chunk from the cache file (bstd `File::read_into`), applies the slide info to it as
plain pointers, maps it and runs the faulting instruction again. Resident guest memory is
capped (`--resident-cap`, default 4 GB); passing the cap stops the run with a typed error,
never an eviction or a retry. The guest may not unmap, reprotect or map over the cache
(`Stop::CacheChange`, by name). The Sentry answers `shared_region_check_np` with the cache's
base: dyld then uses it (UNVERIFIED). Pointer authentication
keys are disabled in `SCTLR_EL1`, so signing and authenticating are identity and plain pointers
work (UNVERIFIED for every instruction class the cache uses). The decoder is checked against the
host's own live shared region: same slide, same non-auth words, same auth words once stripped.
Later: the fixed-up cache chunks become a record shared by every helper, still copied into each
guest's own memory, never mapped from the record's file (Host safety).

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
   the exit protocol with no dyld, cache or pointer authentication in the way. Reached once
   at 15:36 on 2026-11-16 by the code before the incident (from the predecessor's scratch
   output: `hello from a linux guest`, two exits, `exit_group(7)`); the code since the fix has
   never run (no run is allowed until the owner says so).
1. A C `hello` runs to exit in the guest; its stdout and status equal the native run's.
2. `clang -c` on one file; the object is byte-equal to the native run's.
3. One small C-building Cargo build script from the reference project.

Throughout, the real kernel is the oracle (same step with and without hvbox, identical
outputs), and a hostile suite must fail inside the guest, each probing only targets under the
scratch directory: a raw `svc` for a call not on the list, a symlink escaping the view, a Mach
service lookup, an open outside the read set, a write outside the write set.
