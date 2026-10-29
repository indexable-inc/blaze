---
status: current
summary: A map of the code: the worker, the cargo drop-in, the splice, and how a build and an edit flow through them.
---
# How blaze works

This is a map of the code for someone who wants to read or change it. Paths are those of the
published tree: `crates/blaze-engine` is the worker, which compiles and links, over `crates/blaze-cargo`,
the cargo drop-in (commands, the daemon and its client, plans, the sandbox and the remote store, which
call up into the engine only through `blaze_cargo::engine`) and `crates/blaze-splice`, which writes an
edit into the executable of the last link and depends on none of the front end; `crates/hir-ty-codegen` is the code generator
(a crate over `hir-ty`'s MIR, see [contributing.md](contributing.md)), and the front end is
the crates derived from rust-analyzer (`hir-def`, `hir-ty`, `syntax`, `parser` and the rest), with
the borrow checker as its crate `hir-ty-borrowck` ([borrowck.md](borrowck.md)).

## The idea

rustc is a batch compiler. Each invocation reads a crate's source and its dependencies'
metadata, does the whole pipeline, writes files and exits. Cargo decides which crates to
run it on from file timestamps and fingerprints. Incremental compilation saves part of the
work by writing rustc's state to disk and reading it back, per crate, per target directory.

rust-analyzer is the opposite shape. It is a long-running process built on a query database
(salsa) that holds the parsed, name-resolved and type-checked program in memory and
recomputes only what an edit invalidates. It already lowers function bodies to MIR,
monomorphises them, computes type layouts and evaluates constants, because the IDE needs
those for const evaluation and hover.

blaze adds the missing last step, from rust-analyzer's MIR to machine code, and keeps the
process running as a build daemon. An edit then costs what the database has to redo plus
the code of the functions that actually changed.

## The rule

**rustc is the reference.** A program built with blaze's code must behave as the one rustc
built. Where they differ, blaze is wrong.

**Never guess; refuse visibly.** The code generator returns "unsupported" with a reason for
anything it has no explicit lowering for. Under `blaze` a refused function fails the
build, which names it (`cargo/whole.rs`). A refusal is printed, counted
and listed (`--report`, `blaze status`); it is treated as a bug to fix, not as a
normal path. What must not happen is a best-effort lowering of input the generator does not
understand, because that is wrong code nobody sees.

Leaning on rustc under plain cargo is how blaze works today, not how it is meant to:
blaze is to compile everything itself and never run rustc, and each place it still does is a
bug to remove ([no-rustc.md](no-rustc.md)).

The same rule shapes the smaller pieces. `type_id.rs` computes `TypeId` hashes the way the
pinned rustc does and returns `None` for a type whose hash needs something it does not
reproduce. The splice returns an error for anything it has no answer for and the caller
links in full. `blaze` fails a build that needs what it does not model, with the
reason (`refused` in `cargo/whole.rs`), and hands nothing to cargo.

## Processes

    blaze build ──► blaze-engine cargo (client) ──► blaze-engine daemon ──► worker: blaze <project> --socket
                                                        │                │
                                                        └── more workers └── one database

The **client** (`crates/blaze-cargo/src/cargo/`, entry in `mod.rs`) is what `blaze` runs.
It parses the command line (`cli.rs`), decides whether the daemon can serve it (`state.rs`),
and asks the daemon; `build`, `run` and `test` are whole builds (`whole.rs`) and run no cargo.

The **daemon** (`cargo/daemon.rs`) is one process per user on a Unix socket
(`socket.rs`). It loads nothing. It starts workers, passes requests to them, and stops them
and itself when idle. Requests and answers are one JSON object per line.

A **worker** is `blaze <project> --socket <path>`, the long-running mode of `blaze-engine`
(`crates/blaze-engine/src/lib.rs`). It
holds one rust-analyzer database. A worker is keyed by repository, toolchain, target,
feature set and whether libraries are built with `cfg(test)`, because those are inputs of
loading. Checkouts of one repository join the same worker; another repository gets its own,
so a crash or reload of one does not cost the other its state. A worker exits when the
daemon's end of its standard input closes.

The same worker binary is what the harnesses drive directly, without cargo's wrapper:
`blaze <project> --crate <name> --out <object> --symbols <rlib>... --link-cmd <cmd>
--socket <path>`, then `blaze sync <socket>` to ask for a build. There is no `--help`; the
flags are parsed in `lib.rs`.

## The database and what worktrees share

A worker loads a cargo workspace through rust-analyzer's `load-cargo` and
`project-model`, including build script output and proc-macro expansion through
rust-analyzer's proc-macro server. Source changes arrive either from a file watcher or,
under `blaze`, are looked for when a build is requested (`tree.rs` finds added and
removed files by directory timestamps).

A second checkout of the same repository is added to the same database (`--also`, or a
`--project` request to the socket). Crates are identified by their content
(`content.rs`): two checkouts with the same source for a crate have the same crate, and what
was analysed and compiled for one is used for the other. Only the crates that differ, and
the crates that depend on them, exist twice. `harness/worktrees.py` tests this with several
checkouts edited independently under one daemon.

Besides source, a checkout is loaded from its manifests, lock file, cargo configuration,
`RUSTFLAGS` and what its build scripts read. `inputs.rs` checks before each build whether
those are still what was loaded, reruns build scripts where cargo would, and says which
checkouts must be loaded again.

`outline.rs` tells an edit that only changes function bodies from one the rest of the
program can see. After a body-only edit, only what comes from the edited file needs
another look. A body counts as visible when it belongs to a `const fn`, when its type is
part of its signature (`async fn`, `-> impl Trait`), when it is handed to a macro, or when
it declares items.

Within the edited file, only the edited function is lowered again. rust-analyzer lowers a body
from the item it is written in, as a tree of its own whose ranges begin at the item
(`Body::relative` in the fork), so the text around the item moving it changes nothing it is
made from; its IDE source map is that one moved to where the item is. The places panics and
debuggers report (`hir-ty-codegen/places.rs`) are found the same way, as a row and column in
the item, and where the item begins is added when an object is stitched. An edit inside one
function of a 500-function file lowers, infers and compiles that function and lowers no
other: [reports/INCREMENTAL.md](reports/INCREMENTAL.md).

## From MIR to code

The code generator is `crates/hir-ty-codegen`. `codegen.rs` lowers one monomorphised MIR
body to Cranelift IR. Every local lives in memory. Layout comes from rustc's own
`rustc_abi` crate, and calls follow rustc's unstable Rust ABI for the pinned toolchain, so
that blaze's functions and rustc's can call each other. Vtables (`vtable.rs`), `TypeId`s
(`type_id.rs`) and unwind tables (`unwind.rs`, with `unwind/seh.rs` for Windows) have to
agree with rustc's for the same reason: objects, trait objects and panics cross between the
two compilers' code in both directions.

### One function, one result

Compiling a function is a database query (`fragment.rs`). Its result is machine code that
names the functions and data it uses by what they are, not by where an object file put
them. That makes the result depend on the database alone, so salsa can keep it and reuse it
after an unrelated edit, and a build is the stitching of those results into an object.

An instance of a generic function is compiled from the point of view of the crate its name
mentions, not the crate that asked for it (`home.rs`). Otherwise `Vec<u8>::push` would
depend on every file of whichever workspace crate used it.

### The boundary above the producers

`backend.rs` defines the boundary. Above it, the front half has built Cranelift IR for one
function with layout and calling convention already decided. Below it, a producer returns a
`Code`: bytes, the relocations in them, and how to unwind through them. Nothing after that
can tell which producer made the code.

- **Cranelift** compiles the IR directly. It is the backend of a profile with `opt-level` 0
  or 1 (`dev`, `test`).
- **LLVM** (`llvm.rs`, `llvm/lower.rs`, `llvm/emitted.rs`; behind the `llvm` cargo feature)
  is the backend of a profile with `opt-level` 2, 3, `s` or `z` (`release`, `bench`), at the
  profile's level (`s` and `z` at 2). The client reads the profile and starts the build's
  worker with `BLAZE_BACKEND=llvm` and `BLAZE_OPT`, unless they are set already
  (`cargo/backend.rs`, [cargo.md](cargo.md#backends)). It translates the Cranelift IR of
  the function into an LLVM module holding that one function, optimises it at `BLAZE_OPT`, has LLVM write an object into
  memory, and reads the code, constants, relocations and unwind table back out of it.
  Functions are compiled alone at every level, so nothing is inlined across functions. A
  function LLVM has no lowering for is given to Cranelift, with the reason recorded.
- **wasm** (`wasm.rs`, `wasm_object.rs`). Cranelift has no WebAssembly backend, so for
  `wasm32` targets a hand-written lowering turns the same Cranelift IR into a wasm function
  body, and an object writer emits a relocatable wasm object that `wasm-ld` links. No
  unwinding is emitted; the wasm targets are `panic=abort`.

`asm!` blocks are assembled separately and called as functions of their own, as rustc's
Cranelift backend does (`asm.rs`).

### Targets

The target is an input of the process (`target.rs`): `--target <triple>`, else the host.
Everything platform-dependent asks the `Target` for architecture, object format, calling
convention and unwind format, and never asks what blaze itself was built for. The parser
accepts x86-64, AArch64 and wasm32 with ELF, Mach-O, COFF or wasm objects. What has been
run on each is in [support.md](support.md), and how each port works is in
[platforms.md](platforms.md).

## Linking and splicing

blaze does not produce a program from nothing. rustc and cargo build it once, and blaze
replaces the code of the crates it was asked to compile.

**Full link.** blaze writes one object with its functions and runs rustc's own link line
with that object placed ahead of rustc's rlibs for the same crates. Both define the same
symbols and blaze's must win. An ELF linker and `link.exe` can be told to keep the first
definition. Apple's linker cannot, so there `blaze-engine weaken` makes copies of rustc's objects
whose definitions are weak (`weaken.rs`). `symbols.rs` maps Rust paths to the mangled names
rustc used, read from rustc's rlibs and objects.

**Patch.** After an edit the object of the last full build stays as it is and a small
object holding the changed functions is linked ahead of it (`patch.rs`). Anything the patch
cannot express starts a full object again.

**Splice.** Linking still rewrites the whole executable. The splice
(`crates/blaze-splice/src/lib.rs`) leaves the executable of the last full link as it is and
appends the patch: it lays the patch out behind everything the linker loaded, resolves its
references against the executable's symbols, and writes a jump to the new code over the
entry of each replaced function. Calls, vtables and function pointers still arrive at the
old entry and continue to the new code. A static with a new value is written in place when
it fits there. One that outgrew its place (larger, or aligned to more) is a static of the
patch's own, and the patch has everything of the program that refers to it compiled again
to refer to that (`patch::Base::moved`, [#168](https://github.com/indexable-inc/blaze/issues/168)).
Each splice starts from the executable as the linker left it, so splices
do not accumulate; a program built in full onto what rustc linked is spliced into that
executable whole, and the patches after it start from what that left (see "The first edits
patch what rustc linked"). Anything the splice has no answer for is an error and the daemon links in
full; `BLAZE_SPLICE=0` turns it off.

Each object format has its own implementation:

| Format | File | Notes |
|---|---|---|
| ELF | `splice.rs`, `splice/debug.rs` | New segments for read-only data, code and data, new program headers, the unwinder's search table extended. Debug sections are rewritten so a debugger sees the new code. |
| Mach-O arm64 | `splice/macho.rs` | The patch goes where `__LINKEDIT` was, `__LINKEDIT` is written again behind it, and the file is re-signed. Executables with chained fixups are declined, so the daemon's link asks for dyld info (`-no_fixup_chains`). |
| PE x86-64 | `splice/pe.rs` | New sections behind the linker's last, `.pdata` extended for the unwinder. |
| wasm | `splice/wasm.rs` | Nothing jumps: the module is written again with the new function bodies under the same indices. |

### No size is a reason to link in full

No size of the program, of a section of it or of the patch, and no number of edits, may make
an edit link in full. A field the splice writes is either sized so that it always fits, or it
has a layout to fall back on that does not need it. The only limits are the ones the linker
has for the same program. The rules for a change to the splice:

1. An `ensure!` on a distance or a size is a bug unless the linker would refuse the same
   program: then say so where it is checked.
2. A constant room (`Patch::ROOM`, the `ROOM` of `splice/debug.rs`, the 64 MB of
   `Image::tail`) says where something goes first, so that the next splice finds it in the same
   place. It never decides whether something fits: past it, what is behind moves (a debug
   section, what outgrew its room, to behind every other).
3. What a debugger reads is never a reason to link in full. When it can't be written, the
   patch's code has no lines, and the program's are where they were.
4. A new size limit, or a new layout for one, gets a step in `harness/bigbinary.py`, which runs
   nightly on Linux and macOS.

Each place, and how it is covered:

| What | Fixed width | Covered by |
|---|---|---|
| Where the patch goes (ELF) | Every distance from the patch to the program is 4 bytes on x86-64 and AArch64 (calls, the unwind search table's entries, `adrp`) | The patch goes behind the loaded segments, as far into memory as into the file. When the file is longer than four bytes of distance reach (a debug build of bevy is 2 GB past its 200 MB of code), it goes right behind what is loaded instead (#79). |
| The jump over a replaced function | x86-64 `jmp rel32`, 5 bytes. AArch64 `b`, 4 bytes, reaching 128 MB; beyond that `adrp`, `add`, `br`, 12 bytes | blaze makes no function shorter than the longest jump it may need: 5 bytes on x86-64, 12 on AArch64 (`Arch::shortest_function`). Mach-O measures a function to the next symbol, PE by its `.pdata`. A function of rustc's executable (the first edits) is as rustc made it. One shorter than the long jump is linked in full once, by blaze, and every edit after is spliced into that link. |
| Calls out of the patch | AArch64 `bl`, 128 MB | A stub for each call out of reach. |
| The unwinder's search table (ELF) | Entries of 4 bytes from the table | The table is in the patch, within the reach of the placement above. |
| `__unwind_info` (Mach-O) | 24-bit offsets into `__eh_frame` | Three layouts: the patch's frames behind the program's, in front of them, or both tables in the room the header leaves. The last has no limit but that room, which each full link checks. It links again with room for both tables and a quarter more when the room is short, or the program has so many frames that none can go in front of them. |
| Debug info (ELF) | DWARF's 4-byte offsets into a section, 4 GB | Rule 3. |
| The program itself | x86-64 code and data within 2 GB, AArch64 within 4 GB (`adrp`); a Mach-O executable within 4 GB (its signature) | The linker's limits, which the program already fits. The patch is added within the same reach. |

## The disk store

`crates/blaze-store` is a content-addressed key-value store of compiled functions,
independent of Rust: a key is a hash of what the value was made from. There is one per user
and machine, shared by every daemon and checkout, unless `BLAZE_CACHE` names another
directory or `off`. `stable.rs` gives function instances names that are the same in every run on
the same sources, and `cached.rs` keeps entries under them: a function of a crate that no
edit reaches, as it was stitched into an object. A build that finds an entry for an
instance and for everything it calls puts them in the object without asking the database
about the instance.

On disk the store is a set of immutable packs named by the SHA-256 of their bytes, their
values in zstd blocks behind an index, plus one open pack per running daemon. Format,
locking, integrity checks and size limits are in [cache.md](cache.md).

## The remote store

`crates/blaze-cargo/src/remote.rs` and `remote/` keep a local store in step with a directory or
an S3 bucket. Because packs are immutable and named by their hash, a remote is a set of
pack files, a key list per pack, and a manifest per project. `transfer.rs` moves one object
so that the local file is either complete or absent; `s3.rs` is the bucket through the AWS
SDK; `signed.rs` signs and checks manifests; `collect.rs` is garbage collection;
`keeper.rs` is the background task that pulls and pushes while a daemon runs. `deps.rs`
does the same for what cargo built of registry dependencies.

## The cargo drop-in and the unit graph

Since 2026-10-27 ([#408](https://github.com/indexable-inc/blaze/pull/408),
[#571](https://github.com/indexable-inc/blaze/pull/571)), `blaze build`, `run` and `test`
are whole builds: every crate is compiled from source over the shipped standard library and
linked by blaze, with no cargo build and no rustc (`cargo/whole.rs`). This section and the next
describe the model `blaze` had before, a first build by cargo and rustc that the daemon
took over from. Step M of [no-rustc.md](no-rustc.md#the-plan) removed the rest of it
(2026-10-27): the wrapper behind plain cargo after `blaze enable` (`cargo/wrapper.rs`) and the
patches of rustc's executable (`patch::Base::of_rustc`) are gone.

`blaze` needs three things from cargo for every program: the rlibs and objects rustc
built, the line rustc ran the linker with, and which workspace crates are in the program.
It gets them by being cargo's `RUSTC_WORKSPACE_WRAPPER` on the first build
(`cargo/wrapper.rs`). Cargo runs the wrapper in place of rustc for every workspace crate
and for no registry crate. The wrapper runs rustc as asked, adding
`--print link-args=FILE -C save-temps` for crates linked into an executable, and writes a
record of the unit: the arguments, and copies of the files a later cargo build would
overwrite (`cargo/units.rs`). The daemon reads and links against the copies, so a cargo
build beside it never changes a file under it.

From those records `cargo/plan.rs` works out what the daemon is asked to build a program
from: its workspace crates, the files to read symbols from, and rustc's link line with the
daemon's objects ahead of rustc's. `crates/blaze-cargo/src/plan.rs` computes the units of a build
with the features and profile settings of each, which `cargo metadata` does not give per
build.

`cargo/state.rs` records what a cargo build left and everything whose change makes the next
build cargo's again: manifests, lock file, cargo configuration, toolchain, environment
variables rustc reported reading, and build script inputs. `cargo/script.rs` reruns
workspace build scripts the way cargo does and compares what they printed and wrote; if
that is unchanged, the build stays the daemon's.

The daemon assumes the source it loads is the source rustc compiled. The client records the
time just before cargo starts. When a worker has loaded, the daemon checks that no source
file rustc read is newer than that, and discards the worker if one is.

### The first edits patch what rustc linked

The daemon is ready once it has loaded the workspace and read the symbols of the executable
rustc linked; it compiles nothing of its own first ([#11](https://github.com/indexable-inc/blaze/issues/11)).
The first edits are patches of that executable (`patch::Base::of_rustc`), spliced into it as
later edits are spliced into the daemon's own links. To link a body against code it did not
compile, the resolver also uses rustc's private local symbols (`symbols::Linked`), which it
stops using once a build is in full. On macOS the wrapper links those executables with
`-Wl,-headerpad,0x400000` and `-Wl,-bind_at_load`, so a splice has room for its load commands
and the unwind table of a program of some four hundred thousand functions, and every import
is bound at start; the daemon's own links leave both out of rustc's line
(`cargo::plan::rustc_link_args`). The daemon starts a spare worker as it starts, which has
the library of the toolchain loaded before it is told a build; none is started to wait once
a workspace is loaded, unless `BLAZE_SPARE=ready` ([cargo.md](cargo.md#environment-variables)).

A patch over rustc has no marks of before to compare with, so the daemon takes them just in
time: before an edit's text reaches the database, `home` notes the functions written in the
file, their closures and the statics they declare, under rustc's symbols. An `async` block
has no mark; the hash of its function's text and place stands in. rustc copies a generic
function into the crates that call it, line numbers and all, so `home` also takes in the
copies of the file's generic functions that its other functions reach and the executable
has under their paths (`Base::home_copies`, `symbols::Linked::instances_of`): an edit that
changes or moves the lines of such a function is a patch that replaces each of those copies
under rustc's symbol, as long as the executable has no other copy of that name. A function of
the workspace that no function of the executable is named after (`has_no_function_named`)
was not compiled or not linked, so nothing of rustc's calls it: the patch compiles it under a
name of its own, and an edit of it, `#[inline]` or not, is a patch too. Of a function rustc's
symbols name no copy of, `home` keeps a mark, and one whose code is the same where it moved,
as an unused derive below the edit, is no change. An edit that adds functions to a file and
changes nothing else of it but bodies is a patch too when they are not generic or `#[inline]`,
no other crate depends on the file's, and no file of that crate named them before
(`Outlines::adding`): only that crate's code sees them, and `home` takes in every file of it
before the edit, so its marks tell what of it changed. A file not
noted before it changed, any other edit that is more than function bodies, an edit of an
`#[inline]` function the executable has a copy of, of a default trait function, or of a
generic one with copies `home` did not take in (the outline's copied fingerprint tells), a
changed static, and `include!`d or asset
files take a build in full, and the build log says which (`no patch: ...`). So does a patch
the splice can't place: one that calls a libc function the executable does not import, or
has frames that do not fit the header's room on macOS, with the program's own beside them. A thread-local of
the executable is reached as rustc's code reaches it: on Linux x86-64 the general-dynamic
sequence is rewritten to the executable's local one (an offset from `%fs`), on macOS through
the descriptor's slot.

Once nothing waits for a second, the daemon builds its own base in full in the background,
from the store as far as it can; an edit that comes meanwhile interrupts it and is a patch
of rustc's executable, unless the base has begun to link: the edit then waits for that link
and is a patch of the base, as a link once begun has replaced rustc's executable. A worker that holds more than a quarter of the machine's memory while
it builds that base gives it up for good, logs so, and edits go on patching rustc's
executable: what the build computed stays in the database, so the bound is on what the
worker keeps (`BLAZE_OWN_BASE_MEMORY`, a size such as `40G`, or `off`). The daemon's
`BLAZE_MEMORY_MAX` does not stand for it: that stops the worker asked longest ago, never the
one asked last, which is the one building its base. zed's base outgrows it, on
the debuginfo types of [#55](https://github.com/indexable-inc/blaze/issues/55).

An edit is never linked in full for want of a base ([#159](https://github.com/indexable-inc/blaze/issues/159)).
Once the own base is given up, or before it is done, an edit that needs a build in full is
built onto rustc's executable: its names are rustc's where the program has no other
(`Resolver::onto`), and the object, all of the program, is spliced into that executable as a
patch is (`splice::Baseline::spliced_whole`): each of its functions that rustc's has is
redirected, the rest are new, and a function too short for the jump or a name rustc's has
more than one of is one of the new. On x86-64 a function's room for the jump runs over the
`int3`s and `nop`s the linker padded it with up to the next symbol, which is what a getter of
one field needs (`mov`, `ret`: 4 bytes). A patch's function of 2 bytes or more with no such
room hops: a 2-byte `jmp` to 5 bytes of a run of `int3`s between two functions within 127
bytes of it (`Image::pads`), and a `jmp` on from there, both undone with the patch;
`BLAZE_SPLICE_HOPS` has every short function hop. One of a single
byte, or with no run near it, is built in full. The executable is read again in the background with the
object's symbols in it, the base of the patches after, which name against rustc's symbols too
and are spliced onto what the whole splice left, an `#[inline]` function's edit too, since that
object has marks to compare with.
nushell's signature edit with its base given up at 2 GB is spliced whole in 1.5 s (Linux's
full link of it took 1.3 s; the point is macOS's and the largest programs', which take
minutes), and the edits after it splice in about 10 ms. Such a build keeps
nothing in the store: it names rustc's private symbols, which no other link has. rustc's link
on Linux keeps the compiler-builtins functions blaze's code calls (`--undefined`,
`hir_ty_codegen::BUILTINS`), which it would otherwise drop as unused. A splice that fails, of
the whole or of a patch after it (a library function the executable does not import, a
static of the whole that grows), has the daemon start again from what cargo builds and link, as when the
first edits' splice fails: that is what splicing can't do, and an own base links it too. Only ELF is spliced whole; on macOS
such an edit still links in full, and the link takes the room for splices that the executable
there wanted (`headerpad_ahead`), so it is not linked twice.

An alternative wrapper exists in `stub.rs` (`BLAZE_WRAPS_RUSTC`), where rustc compiles only
a stub for each workspace crate and all code is blaze's. It makes the first build faster,
but a function blaze refuses then has no rustc copy to fall back on, so it is used by
`harness/alone.py` and not by `blaze`.

## Debug info

`debuginfo.rs` writes DWARF 4 for blaze's functions: line tables, a subprogram entry per
function, and under Cranelift the parameters and locals. Debug info is written for ELF
targets. It is not written for Windows, and has not been checked on macOS. See
[support.md](support.md).

## blaze in the browser

`crates/blaze-wasm` is an experiment that builds the compiler itself for wasm and runs it in
a web page. It is excluded from the workspace and is not part of the daemon. See
[reports/WASM.md](reports/WASM.md).
