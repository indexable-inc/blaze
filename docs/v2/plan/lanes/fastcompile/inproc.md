---
status: plan
summary: blaze v2 lane fastcompile, idea 6 (no link and no rewrite for dev runs): crates/blaze-inproc, the dev executable written once per content key to cargo's path and started by the kernel, a start hook linked into the program that lays the daemon's code memory over its code before main with every byte equal at the swap, linked addresses and the slide, the handshake and its checks against the image's own headers, fences for running threads (epochs, stack walks, the OS thread list, model-checked), the arena allocator, the channel's frames, the run's lock-free asks (shared slots and a futex, model-checked), the platforms, the requests of link, cargo, usercode, core and the coordinator, and the speed of light.
date: 2026-10-29
---
# fastcompile: no link and no rewrite for dev runs (`crates/blaze-inproc`)

Idea 6 of [compile-time.md](../../design/compile-time.md). It was "no binary on disk"; it is
now **no link and no rewrite**: a dev run's or test's program is a real executable at the
path cargo gives it, written once per content key and never again for a patch, and the code
it runs is the daemon's code memory, laid over the file's code before `main`. A patch is a
write into the daemon's view plus one atomic store, as before. Owned here:
`crates/blaze-inproc/`, this file and [inproc-windows.md](inproc-windows.md). Hooks in other
lanes are requests in `v2/requests/fastcompile.md` (`fastcompile-inproc-exec 1` and on; the
earlier `fastcompile-inproc` and `fastcompile-inproc-macos` requests are superseded there).

## Why the change

blaze's own loader (`blaze-inproc-run`, removed) mapped the image, relocated it, set up
thread-locals and ran it in a process whose executable was the loader. Every reader of "my own
executable" saw the loader: `current_exe`, `/proc/self/exe`, dyld's image 0, the PEB,
`GetModuleFileName`, `dl_iterate_phdr`, and with them backtrace symbolization and anything a
program finds beside itself. Fixing each one was a list that never ends, and superset-of-rust
allows none of them to differ. Now the kernel and the system's dynamic linker start the
program from its own file, exactly as in a whole build, so they are all right by
construction. What blaze adds is one swap of mappings whose bytes are equal.

## How a run starts

1. **The file.** `host::Host::write_executable` writes the image part of the code memory (the
   linker's file, byte for byte) to cargo's path, keyed by the store's content hash: nothing
   when this host wrote that key there and the file is unchanged since (same length, mtime,
   device and inode), else a clone of the store's file (`FICLONE`, `clonefile`) where the
   filesystem has clones, else one write. Always into a new file renamed over the path, so a
   process still running the old one keeps it. It is never written from a code memory patched
   since it was laid out (`CodeMemory::is_pristine`): that is an error, never a file that
   differs from what the overlay swaps in.
2. **The spawn.** `host::Program::command` (or `Host::launch`) makes a `blaze-core` `Command`
   for `Effect::Spawn`: the program is the file itself, in the run's sandbox, which gains the
   file to read and the host's socket to connect to. `BLAZE_INPROC` names the socket and a
   128-bit random token.
3. **The start hook.** The program's runtime (`runtime.rs`, linked into the executable) has
   `blaze_inproc_start`, and the link puts a pointer to it first among the executable's
   initializers: ELF `.preinit_array`, Mach-O `__mod_init_func`, a PE TLS callback in
   `.CRT$XLAB`. Only the dynamic linker's own work runs before it (mapping, relocating, the
   executable's IFUNC resolvers, which therefore must be resolved eagerly, never lazily).
4. **The handshake** (`handshake.rs`). The hook takes `BLAZE_INPROC` out of the environment
   (on Linux out of `envp` in place, since libc's `environ` is not set yet at preinit), so
   neither the program nor its children see it. It connects and says HELLO: the token, its
   process ID, the version, the page and where its image's header is. The host checks the
   token names a waiting launch, the process ID is what the OS says of the peer
   (`SO_PEERCRED`, `LOCAL_PEERPID`, `GetNamedPipeClientProcessId`) and the peer is this user,
   then answers GRANT: the format, the linked header and span, the objects (the image and
   arena's object, the fence's control words, the asks, the cells, the objects of fixed maps)
   and the overlays.
5. **The checks.** Before mapping anything the hook reads its own image's headers as the system
   loader mapped them (`image::layout`) and checks the grant against them, each relative to its
   own header: the same span; every overlay whole pages; an overlay over code is exactly one
   of the image's code segments, from the same file offset of the image object, executable;
   every other overlay is inside a reservation the executable carries; every code segment is
   covered. Decoding already refused any object or overlay a hook must not map: the image
   object shared writable, the control words, asks or cells executable, an overlay of the
   control words or the asks, code from any object but the image's, anything outside its
   object or the span, overlays out of order or overlapping.
6. **The swap.** The hook maps the control words and the asks, then replaces each overlay's
   pages with the mapping of its object at the linked address plus the slide (Linux `mmap`
   `MAP_FIXED`, macOS `mach_vm_map` with `VM_FLAGS_OVERWRITE`; on Windows the daemon rebuilds
   the image's region, below). The bytes are equal at the moment of the swap: the file was written from the
   object, and the daemon writes only code no thread can reach yet.
7. **The dependency images' open words** (`bind.rs`, queue item 01032). `host::Launch` carries
   `blaze_premap::start::Start`'s writes and lookups (`ImageWrite`, `ImageLookup`), and the GRANT
   carries them after the overlays: each write's place, value and whether the value is an
   address; each lookup's place, addend, weak flag and name (names pooled in one string, at
   most `MOST_NAME` bytes each, at most `MOST_BINDS` of either; every count is checked against
   the bytes left before anything is allocated). Every overlay of a fixed map is flagged a
   dependency image's; decoding refuses the flag on anything else, and refuses a write or
   lookup that is not 8 bytes whole inside a flagged `Access::Copy` overlay. Before mapping
   anything the hook refuses a slide other than 0 while a flagged map is in the grant, naming
   it: the image's words that name its own addresses are linked at its base, and blaze-link
   lists none for rebasing yet. Once the overlays are mapped it stores each write at
   `place + slide` (the value plus the slide when it is an address, never for a thread-pointer
   offset), then resolves each lookup in the process's global scope (`sys::Scope`:
   `dlsym(RTLD_DEFAULT, ..)` on Linux and macOS, the C name, so a Mach-O name's leading `_`
   taken off; `GetProcAddress` over `K32EnumProcessModules`' modules in load order on Windows)
   and stores its address plus the addend. A weak miss stores 0. A strong miss is never stored:
   the start fails with exit 127, naming the names (the first eight and how many more). Every
   store is one unaligned 8-byte write (`sys::store_private`) into the run's private copy
   (`MAP_PRIVATE`, the anonymous copy on macOS, a copy-on-write view on Windows), never the
   memory object. The hook says STARTED and returns; the C runtime, the program's initializers
   and `main` run as anywhere.

   `host::Program::command_with` takes the maps, writes and lookups (`Program::command` is it
   with none); the caller that holds `blaze_premap::map::Memory` and its `Start` passes them.

A hook that can't attach says FAILED (when the channel is up), prints why on stderr and exits
with 127: a run never runs the file's code in place of the object's. A program started with no
`BLAZE_INPROC` (run by hand) runs its file as it is: `runtime::attach` says so and
`runtime::quiesce` does nothing. A link without the hook pointer makes `attach` say the hook
never ran.

## Addresses

Every address the crate deals in is *linked*, as the linker placed it. Each run process has its
own slide (ASLR), the same for the whole image: where its header is minus the linked header
(`image::Layout::header`). The arena, cells and fixed maps go into reservations the executable
carries in its own address range (first the arena, then the cells, then fixed maps): an ELF
`PT_LOAD` with no flags and no file bytes, a Mach-O segment of `initprot` 0 and no file bytes, a
PE section with no access and no raw data. So they slide with the image, and:

- bytes every process shares (code, the arena, stub words, cells) must not depend on the slide:
  code is position-independent and a word naming code names it relative to itself;
- `CodeMemory::linked` gives an offset's linked address; a process has it at that plus
  `host::Session::slide` (the daemon's view) or `runtime::Attached::slide` (the process's own);
- the fence's ranges are linked; each thread subtracts its slide from its return addresses
  before comparing.

## The state of the art, and where this stands

- **Dual mapping.** .NET 7's W^X mode and Firefox's JIT map one shared-memory object twice,
  writable and executable, and still flip protections or remap per write in places. Here the
  writable view is in another process (the daemon) for good, so there is no protection change
  after the start, ever: a write is a `memcpy`, a patch is one release store. On AArch64 the
  publish is the architecture's minimum (`dc cvau`/`ic ivau` per line only where `CTR_EL0`'s
  `IDC`/`DIC` say they're needed, one `dsb ish`, one `isb`).
- **Hot patching a running program.** Live++ and Unreal's Live Coding load a patch DLL and hook
  functions; Dioxus's `subsecond` loads a patch dylib and swaps a jump table. Each goes through
  a file and the system loader per patch. Here a patch goes through neither; only the first
  start of a layout touches a file, and that file is the program itself, so nothing about the
  process differs from its whole build.
- **Starting from a file, running from memory.** Checkpoint/restore and some JITs remap a
  running image (CRIU's restorer, `PR_SET_MM_MAP`); none do it before `main` with the bytes
  proved equal. Here the swap is checked against the image's own headers and the bytes are
  equal by construction, so the program can't tell the moment it happened.
- **Reclaiming replaced code.** Userspace RCU's QSBR flavor frees after every *registered*
  thread passes a quiescent state; the Linux kernel's livepatch consistency model switches a
  task once its stack holds no frame of the old function. The fence here does both: epochs
  reported at quiescent points, refined by a stack walk (a thread that never entered the
  replaced code does not hold it), and threads that never registered made safe by counting
  the OS's list of threads against the slots (`births` guards the count). It is checked
  exhaustively (below), which neither publishes for its implementation.
- **The arena** is TLSF (Masmano et al., 2004): constant-time take and give, neighbours merged
  through start and end maps hashed with one multiply.
- **The channel** decodes frames in place from one reused buffer (no copy, no allocation per
  frame), with fixed little-endian headers and every read bounds-checked.
- **Asks from the run** go through slots in shared memory and a futex, as io_uring's rings
  and futex-based locks share memory across a boundary: no lock and no frame per ask, so a
  signal handler that interrupts its own thread's ask asks too, which a lock around a framed
  stream can't allow (below).

## The pieces

### Code memory (`lib.rs`, `sys/`)

A `CodeMemory` holds three parts at offsets `0..len()`: the program image as the linker lays out
its file (a file offset is an offset here), the arena after it, TLSF-allocated (`alloc.rs`) for
code placed while the program runs (`blaze-lazy`'s functions), and the cells after that, words
shared both ways. The image and arena are one OS object (memfd, Mach memory entry, pagefile
section); the cells are a second, so the image's object is never granted writable. The daemon
maps both once, shared and writable. `lay_out` reads the image's headers and places the arena
and cells in the executable's reservations; it marks the memory pristine, and any write, fill
or store into the image part clears that.

Changes: `write` (bytes nothing can reach yet), `publish` (the instruction-cache maintenance),
`store_word` (one aligned release store of a slot or a stub's target word; x86-64 and AArch64
stubs load it) and on AArch64 `store_instruction` for the instructions Arm lets one thread
change while another runs them (`B`, `BL`, `NOP`, `BRK`, `ISB`, `SVC`; Arm ARM B2.2.5).

### The image's headers (`image.rs`, `image/`)

One reader per format gives the code segments (file offset, linked address, length), the
reservations and the linked header, and refuses what a run can't overlay whole pages of: a code
segment writable, with bytes past its file bytes, off a page, sharing a page with anything
else, or (PE) off the 64 KiB granule. The daemon reads the file's bytes, the hook its own
headers in memory; both are untrusted bytes, and `image/tests.rs` fuzzes mutated and cut
headers (an error or a layout, never a panic).

### Fences (`fence.rs`)

A page of words shared by the daemon and every run process (the control object): the epoch
(the daemon's), a table of 256 ranges being retired by linked address with their grace,
1024 thread slots of 128 bytes each (written by their thread only: process, thread ID, `seen`),
and `births`. `retire` posts a range unlinked by stores already made under grace `g`, then the
epoch becomes `g + 1`. At a quiescent point (`runtime::quiesce`, called where `blaze-live`
already polls, and at a thread's end through a thread-local's destructor) a thread reads the
epoch `e`, and while it has not changed since its last report that is all it does (one
load). Otherwise it serializes (`isb`; `cpuid`), walks its stack through the unwinder, takes
its slide off each return address, and reports `e`, or the oldest grace of a posted range one
of its return addresses falls in. A range is freed once every live thread of every process
has a slot past its grace: the daemon reads `births`, lists each process's threads from the OS,
reads the slots, reads `births` again, and trusts the slots only if `births` did not move and
each process has exactly as many online slots as listed threads. Slots of threads no longer
listed are reaped; a thread whose ID the OS reused adopts the slot its dead namesake left, so a
long-lived thread never stalls the count.

What the fence rests on, which `blaze-lazy` keeps: replaced code is reachable only through
slot and stub words and from stacks. A function pointer the program holds is a stub's address,
never a body's, and no quiescent point is between a stub's load of its word and the jump.

A process that forks children running the program (a test binary's fork server) is attached
with `attach_forking`: a child exists before anyone can tell the fence of it, so nothing
retired is freed until the server and its children have exited. The arena then fills with an
error naming the process, never a reuse.

Proof (`fence/model.rs`): an explicit-state model of the daemon and up to three threads at the
grain of single shared accesses, every interleaving explored (sequentially consistent, which
is what the code's `SeqCst` accesses and the fence before the epoch store give), including
threads started while the daemon lists, a thread list that may or may not include threads
born or ending during the listing, threads dying with their slot held, thread IDs reused, and
more threads than slots. Checked: safety (when the range is freed no thread is in it or about
to call into it), liveness (from every reachable state a state with the range freed is
reachable), non-vacuity (it is freed while a thread runs), and teeth: dropping the count, moving
the epoch before the unlink, or ignoring the stack is each caught. `fence/tests.rs` checks the
real code's rules one by one and stress-tests it: threads calling through a slot the daemon
keeps moving while it poisons every range the fence frees, with thread churn, reused IDs and
threads dying with their slot held; a thread reading poison fails the test.

### The channel (`frame.rs`, `channel.rs`)

12-byte little-endian headers (kind `u16`, zero `u16`, length `u32`, sequence `u32`), bodies
of at most 64 MiB, kinds partitioned by crate (`0x000` inproc, `0x100` `blaze-lazy`, `0x200`
`blaze-premap`). Every read is bounds-checked; a short buffer, reserved bits, a long body,
trailing bytes, bad UTF-8 or a count larger than the bytes left are each an error, never a
panic or an allocation for bytes that are not there (fuzzed in `tests.rs`, with the HELLO and
GRANT bodies).

### The asks (`ask.rs`)

Once it runs, the program asks the daemon for things (`blaze-lazy`'s code on a lazy function's
first call) with `runtime::Attached::request_into(kind, body, answer)`: any number at once,
from any thread, from TLS destructors, and from a signal handler that interrupted its own
thread in the middle of an ask. It takes no lock, allocates nothing until it fails, and leaves
`errno` (the last error on Windows) as it found it.

Each launch has an object of its own for them (object 2 of the grant, words, mapped shared by
both ends): a header (a bitmap of claimed slots, a count of slots given back, a count of
threads waiting for one, and what the run names its bells by), then 64 slots of 4 KiB. A slot's
state word is a 24-bit generation and a phase (free, asked, taken, answering, answered), so it
fits the 32 bits a futex compares. The asker claims a slot with one compare-and-swap on the
bitmap, writes the kind, length and body, stores *asked* and rings the doorbell; it sleeps on
the state word until *answered*, copies the answer into its buffer (the answer's whole length
says when it did not fit), and gives the slot back under the next generation. The daemon's
`ask::Reader` wakes on the doorbell, takes each asked slot (*taken*, one compare-and-swap) and
hands it out as a frame whose sequence number is the slot and its generation; an `ask::Sender`
(`Send + Sync`, cloned to any thread) takes the slot to *answering* (compare-and-swap: once,
and never for another generation), writes the answer, stores *answered* and wakes the slot.
`host::Session::split` gives the reader and the sender; `Session::kill` and `Sender::kill` end
the process, the first reason kept as the run's error (`killed`).

| Platform | Asker sleeps on | Doorbell | Kill |
|---|---|---|---|
| Linux | a futex that is not private, so the same in both processes | one byte on the socket | `pidfd_send_signal` on a pidfd opened at the grant |
| macOS | `__ulock_wait` with `UL_COMPARE_AND_WAIT_SHARED` | one byte on the socket | `kill` of the process ID, so only before the launcher reaps it |
| Windows | the slot's auto-reset event, which the daemon made and duplicated into the run | the doorbell event | `TerminateProcess` |

Neither end sleeps at once. Before each sleep an asker spins on its state word for 3 µs,
about one cross-process wake-up, so spinning never costs more than the sleep and wake would.
The spin reads only the words and the clock, so it stays async-signal-safe. The reader rescans
the slots for 50 µs after the last ask it took before it sleeps on the doorbell. Rings sent
while it spins wait in the socket (or the event) until its next sleep drains them. A burst of
first calls from one thread therefore pays no wake-up (the
[first call](lazy.md#speed-of-light)'s two, about 6 µs); the reader's spin costs one daemon
core for 50 µs after a burst. An asker whose ring finds the socket's buffer full waits for
that next sleep, at most 50 µs after the last ask was taken; its own ask is taken meanwhile.

A thread that finds all 64 slots claimed sleeps on the count of frees (in the process: a
futex, `WaitOnAddress` on Windows). Other threads' slots come back as they are answered, so the
one way to wait for ever is 64 asks nested on threads their own signal handlers interrupted.
An asker whose daemon is gone wakes every second, finds the channel hung up and fails. The run
is not trusted: the daemon reads each length once and bounds it to the slot, and refuses an
answer to a slot not taken under that generation.

Proof (`ask/model.rs`): an explicit-state model of the slots, the bitmap, the frees count and
the waiters, with askers, one of them interrupted by a signal handler that asks inside its
ask, a reader and a sender, at the grain of single shared accesses; every interleaving is
explored. Checked: each asker gets its own answer, from every reachable state every asker can
finish, the handler finishes while its thread is inside its ask, and with one slot it waits
for ever exactly as stated above; a lock around the ask, a check-then-set claim or a sleep
that does not compare the word first is each caught. `ask/tests.rs` runs the real code on
Linux: 128 threads asking at once over 64 slots, a signal handler asking from inside its
thread's ask (answered while that ask still waits), a long answer, stale and repeated answers,
a hang-up, a bad length and a kill.

## Platforms

- **Linux x86-64 and AArch64**: a `memfd` sealed against growing and shrinking, mapped
  `PROT_READ|PROT_WRITE` shared in the daemon. The hook gets the image's object as a read-only
  reopen over `SCM_RIGHTS` (so it can't be mapped writable shared) and the words objects as
  is; each overlay is one `mmap` `MAP_FIXED|MAP_SHARED` (`MAP_PRIVATE` for a copy).
  `vm.memfd_noexec=2` is an error naming the sysctl. AArch64 checks `CTR_EL0` at
  `CodeMemory::new`: an instruction cache that aliases by virtual address (VIPT with ways
  larger than a page, or AIVIVT) is an error, since code is written through one mapping and run
  through another. Threads from `/proc/<pid>/task`. The image's header is the first
  `dl_iterate_phdr` entry's `PT_LOAD` at file offset 0. Exec of the file is plain `execve`: the
  file has no writable mapping anywhere (the daemon writes a new file and renames it), so no
  `ETXTBSY`.
- **macOS AArch64** (`sys/macos.rs`, `sys/macos/rendezvous.rs`): a Mach memory entry
  (`mach_make_memory_entry_64`, `MAP_MEM_NAMED_CREATE`, R|W|X), each grant a sub-entry of the
  rights it allows. Each overlay is one `mach_vm_map` with `VM_FLAGS_FIXED|VM_FLAGS_OVERWRITE`;
  a copy is anonymous memory filled from a read mapping, since a mapping can't gain rights its
  entry lacks. What the system allows, from XNU and dyld:
  - a POSIX shared-memory mapping is never executable, so it has to be a memory entry;
  - W^X holds per mapping, not per object: the daemon's R|W and the run's R|X mappings of the
    same pages are allowed;
  - unsigned executable pages kill a process only under `CS_ENFORCEMENT`, which the hardened
    runtime sets; so the dev executable is signed ad hoc without the hardened runtime (request
    to link), and the hook reads `csops` first and stops with an error naming it instead of
    being killed;
  - `CTR_EL0` traps at EL0, so the instruction-cache check reads `hw.optional.arm64`, and a
    publish is `sys_icache_invalidate`.

  A Unix socket can't carry Mach ports, so the entries go through a bootstrap service the
  daemon checks in (`dev.blaze.inproc.<daemon pid>`, as Chromium's rendezvous does), each side
  checking the other's audit token against the channel's `LOCAL_PEERTOKEN`. A sandboxed run
  must be allowed to look it up (request to core). The image's header is
  `_NSGetMachExecuteHeader`. Unverified until a Mac run: whether the code-signing monitor on
  newer Apple silicon polices pages of an ad hoc signed process replaced by `VM_FLAGS_OVERWRITE`,
  and `bootstrap_check_in` of an undeclared name.
- **Windows x86-64** (`sys/windows.rs`, `sys/windows/pipe.rs`): a pagefile-backed `SEC_COMMIT`
  section the daemon maps into the run process itself (`MapViewOfFile3` on the process handle),
  so the run process never holds a handle it could map writable. An image section's view can't
  be partly replaced, so on the hook's OVERLAY the daemon rebuilds the image's region while the
  hook waits on the pipe: it reads every committed page outside the overlays, unmaps the image,
  reserves its span as one placeholder, splits it at the overlays, maps each overlay's view
  (`MEM_REPLACE_PLACEHOLDER`), and puts the pages it read back as private memory with their
  protection. A failure past the unmap ends the process. Details in
  [inproc-windows.md](inproc-windows.md).
- Elsewhere every constructor stops with an error naming the platform (`sys/none.rs`).

## What a program sees of itself

Its executable is its own file at cargo's path: `current_exe`, `/proc/self/exe`,
`_NSGetExecutablePath`, dyld's image 0, `GetModuleFileName(NULL)`, the PEB, `dl_iterate_phdr`,
`dladdr` and the backtrace symbolizer (which reads the file's debug info) all name it, as in a
whole build. `crates/blaze-inproc/programs/exact.rs` (`inproc-exact`) checks this in a real
dev run: `current_exe` names the path, the hook attached and took `BLAZE_INPROC` out, and a
backtrace names the program's own functions.

What still differs, stated here rather than hidden:

- **Software breakpoints in code ranges.** A debugger's write into code goes through the
  run's read-only shared mapping and is refused (Linux `ptrace` `POKETEXT`, macOS
  `mach_vm_write`, Windows `WriteProcessMemory` on a view without write access). Hardware
  breakpoints and everything read-only work. The way out is a debugger that writes through the
  daemon (the agent API), not a writable mapping in the run.
- **Mapping names.** `/proc/self/maps` names the code ranges `/memfd:<name> (deleted)`, not the
  file; macOS `vmmap` shows a shared memory region; Windows `VirtualQuery` says `MEM_MAPPED` and
  `MEM_PRIVATE` where the image was `MEM_IMAGE`. Module lists, unwinding and symbolization don't
  read these.

## Wiring (requests in `v2/requests/fastcompile.md`)

- link: the start hook's pointer first among the initializers; code segments on their own
  pages (PE: 64 KiB granules), never writable; reservations for the arena, cells and fixed
  maps; IFUNC resolved eagerly; no base relocation into code; Mach-O signed ad hoc without the
  hardened runtime; the image committed into a `CodeMemory` as the bytes of the file it would
  write.
- cargo: the dev `run` path keeps the executable's path, makes a `host::Program` with the
  store's key and spawns through `Program::command`; `blaze build` and release unchanged.
- usercode: unchanged calls (`Program::command`, `Program::exited`); a test binary's path is
  where its executable goes.
- core: the sandbox lets a run read and exec its executable, and on macOS look up the
  rendezvous service.
- coordinator: no `blaze-inproc-run` to install; the `windows-sys` features; the line in
  compile-time.md's change 6.

## Speed of light

- **A patch** is a write of the new code into the arena, `publish`, and one release store of a
  stub's word: no syscall, no file, no link. Nothing faster is possible without the program
  checking for patches itself.
- **The first start of a layout** writes the file once: a reflink is a metadata update (no
  bytes copied); a write is a `memcpy` into the page cache at memory bandwidth, about 10 ms for
  a 100 MiB debug executable, and never fsynced (nothing needs it durable; a lost file is
  written again from the key). Every later start with the same key does no I/O at all: one
  `stat` of the path.
- **Each start** costs what a whole build's start costs (exec, the dynamic linker, its
  relocations) plus one round trip on a Unix socket (tens of microseconds), one `recvmsg`
  with the descriptors, and one `mmap` per overlay (a code segment or three, the arena, the
  cells, any fixed maps), each mapping pages already resident in the daemon's object, so the
  later faults are minor. On Windows the rebuild adds a `ReadProcessMemory` and
  `WriteProcessMemory` of the image's pages outside the code (its data, typically a few MiB:
  about a millisecond) and a dozen placeholder calls.
- **Against the old loader** this does strictly less per start: no relocation pass of our own,
  no thread-local setup, no import resolution twice. The system's dynamic linker already does
  them, and does them once.
