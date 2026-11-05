---
status: record
summary: What blaze keeps on disk against cargo on four projects, part by part, and the store before and after zstd.
date: 2026-10-23
---
# What blaze keeps on disk, against cargo

2026-10-23. Measured on dev-aws-14 (c8a.48xlarge, `/home` on XFS, which shares blocks between
reflinked files), Cranelift, `harness/bench.py` with `--keep`, rustup's 1.99.0. Each claim is
**read** (measured, or read in the source), **reported** (someone says so) or **unverified** (our
inference). The first pass (the store, #131 and #136) is at the end; the second pass, on
`origin/main` at 1c4cbb9 against the branch that loads build scripts from cargo's messages and
sends packs compressed once (#153), then main at 04bdd5b against the one that keeps `blaze.o`
packed, is next. The third pass (main at 9cd2512 with #175's formatting against the splice that keeps one copy of
the debug sections, and the daemon's program against rustc's) is first; its ext4 runs are on
an ext4 image mounted on the same host.

## Short version

- The splice keeps one copy of what a debugger reads: the linker's is let go of (its blocks
  punched) once copied behind the image. Where nothing is reflinked (ext4) bevy's target is
  23,487 → 21,930 MB and helix's 5,471 → 4,964, its edits as fast; on XFS, where the copy
  shared the linker's blocks, the same (**read**).
- The daemon's program larger than rustc's (helix 748 MB against 424) is rustc's objects
  linked beside blaze's: blaze's own code and debug information are about 240 MB of it,
  the rest rustc's, again. G links none of rustc's (**read**). Merging the duplicated strings
  saves 37 MB of helix's and costs a third of a second a full link: not shipped.
- Compressed debug information stays off (below).
- The 31.6 GB measured for bevy in the first pass was a run whose edits all fell out of the
  splice to a full 300k-function compile, which wrote a 2.4 GB `blaze.o` and 11.8 GB of
  `.blaze-units`. On main at 1c4cbb9 bevy's edits splice, and its target is 16,241 MB against
  cargo's 17,550: already below cargo on XFS (**read**).
- On XFS the pieces F/G removes (`.blaze-units` and rustc's program kept) free nothing on
  bevy, where every block is shared with cargo's rlibs or the daemon's program, and 53-538 MB
  on the others, where the daemon linked a program of its own (**read**, FIEMAP). On bevy they
  cost 6.2 GB only where nothing is reflinked (ext4) (**read**, `du`).
- `blaze.o`, the whole program as the daemon compiled it, is kept zstd-packed between the
  links that read it: ripgrep 532 → 503 MB, helix 5,022 → 4,534, nushell 6,181 → 5,774
  (**read**).
- After F/G, projected on XFS: ripgrep 450 MB against cargo's 407, helix 4,110 against 3,713,
  nushell 5,236 against 5,343, bevy 15,771 against 17,550. What is left over cargo's is the
  daemon's program being larger than rustc's (helix 748 MB against 424).
- rust-analyzer built every build script and proc macro again in `target/rust-analyzer`
  (471 MB on bevy). The daemon now loads them from what cargo said of its build, and builds
  none: bevy 16,241 → 15,771 MB, its warm-up 30.2 → 20.2 s (**read**).
- Packs sent to a remote were zstd again over zstd blocks. They are sent as they are now.
- Compressed debug information: zlib is the only one every reader takes (std's backtrace, gdb,
  lldb, addr2line); lldb 20 cannot read zstd. On bevy's program it is 2,021 → 768 MB, but a
  panic's backtrace inflates it all (~4-6 s, ~1.6 GB) (**read**). Off, decided 2026-10-23:
  the panic's cost is paid in the edit loop.

## After F/G, projected

bevy after the bench's edits, MB. "XFS" counts every block once (FIEMAP); "no reflinks" is
`du`, what each part costs on ext4, where every copy is a copy (**read**, both measured on the
branch's target). F/G's column is the part removed and nothing else (**unverified**: it
assumes F/G changes neither `deps` nor `incremental`).

| part | what it is | cargo's | blaze XFS | blaze no reflinks | after F/G, XFS | after F/G, no reflinks |
|---|---|---:|---:|---:|---:|---:|
| `debug/incremental` | rustc's incremental state; blaze's is smaller as the daemon, not rustc, builds the edits | 8,466 | 6,505 | 6,651 | 6,505 | 6,651 |
| `debug/deps/*.rlib` | cargo's rlibs, the same | 5,886 | 5,887 | 5,887 | 5,887 | 5,887 |
| `debug/.blaze-units/*.rlib` | the rlibs blaze links in place of rustc's (F/G) | none | 0 (4,215 shared with `deps`) | 4,216 | 0 | 0 |
| `debug/examples` | the program: cargo's, or the daemon's with its spliced code and a second copy of the debug sections | 2,016 | 2,197 | 3,749 | 2,197 | 3,749 |
| `blaze/<build>/programs/*/cargo` | rustc's program, kept for the first edits' splices (F/G) | none | 0 (shared with the daemon's) | 2,016 | 0 | 0 |
| `blaze/<build>/scripts` | what cargo said of its build scripts and proc macros | none | 0.03 | 0.03 | 0.03 | 0.03 |
| `target/rust-analyzer` | rust-analyzer's own build scripts and proc macros (gone) | none | 0 (471 on main) | 0 | 0 | 0 |
| `blaze/<build>/programs/*/blaze.o` | the program as blaze compiled it, when the daemon links its own (none on bevy) | none | 0 | 0 | 0 | 0 |
| `deps/*.rmeta`, `*.so`, `build`, `.fingerprint` | cargo's, the same | 1,177 | 1,176 | 1,058 | 1,176 | 1,058 |
| **total** | | **17,550** | **15,771** | **23,577** | **15,771** | **17,345** |

So on XFS blaze is 1.8 GB below cargo today and F/G does not move it. Where nothing is
reflinked F/G brings it from 23.6 GB to 17.3 GB, just below cargo, and the program's second
copy of its debug sections was what is left over cargo's. The splice keeps one copy now
(below): measured on ext4, `debug/examples` 3,657 → 2,083 MB, and after F/G about 15.7 GB.

All four, MB, XFS (`bench.py`'s `disk_mb` and the breakdowns of the kept targets, **read**).
"main" is 1c4cbb9, before this pass; "scripts" is #153 (build scripts from cargo's messages);
"packed" is `blaze.o` kept packed (below), measured against main at 04bdd5b on the same
host and cores (main there: ripgrep 532, helix 5,022, nushell 6,181). "F/G frees" is what
removing `.blaze-units` and rustc's program kept would free, and "after F/G" what is left
of "packed" then.

| project | main | scripts | packed | cargo | F/G frees | after F/G | `blaze.o` | the daemon's program against cargo's |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | 533 | 533 | 503 | 407 | 53 | 450 | 35 → 6 | 83 against 53 |
| helix | 5,022 | 5,010 | 4,534 | 3,713 | 424 | 4,110 | 572 → 93 | 748 against 424 |
| nushell | 6,175 | 6,176 | 5,774 | 5,343 | 538 | 5,236 | 469 → 75 | 817 against 538 |
| bevy | 16,241 | 15,771 | 15,771 | 17,550 | 0 | 15,771 | none | 2,197 against 2,016 |

bevy's "packed" is not measured: it has no `blaze.o` to pack, its first edits being spliced
into rustc's program. After F/G it will have one, about 2.4 GB as the first pass saw it, a
sixth of that packed, and its own program in place of rustc's (**unverified**).

On ripgrep, helix and nushell the daemon's spare worker loads the workspace while cargo builds,
from cargo's messages as they come, so main had no `target/rust-analyzer` there either; bevy's
worker loaded after the build, which built them again. After F/G what is left over cargo's
is the program the daemon linked being larger than rustc's (its code and debug information,
Cranelift's) and the packed `blaze.o`, against an `incremental` smaller than cargo's: helix
397 MB over, ripgrep 43; nushell 107 under, bevy 1,779 under.

## One copy of the debug sections

- Before (**read**, `splice.rs`, `splice/debug.rs` at 9cd2512): a splice appended code behind
  the image and copied the sections a debugger reads behind that code (64 MiB of room),
  pointing the section headers at the copies. Each splice copied them again from the
  linker's offsets, which were never written, so the program held the sections twice: on XFS
  the copy was `copy_file_range` reflinks of the linker's blocks (181 MB on bevy), with no
  reflinks the sections again (1,553 MB).
- Now (**read**, the same files): when the program is first laid out the sections are copied
  behind the room, the linker's section header table is rewritten where it is to point at
  the copies, and the linker's ranges are punched (`fallocate` `PUNCH_HOLE`, the file's size
  kept). A section moves only to space past every other's room, never over a copy: when what
  a patch adds to it outgrows its room, or when the appended code reaches it. Its old range
  is punched once the move is written. Undo is the journal it was (`self.undo`), of the
  ranges a splice overwrote, and no splice overwrites a copy it reads. Where `fallocate`
  cannot punch (a filesystem without holes) the linker's ranges are kept, as before; a
  Mach-O program is spliced otherwise and not changed.
- The ETXTBSY copy of the program, made when a running program holds the file, copies its
  data runs only (`SEEK_DATA`/`SEEK_HOLE`), so a punched range stays a hole there too.

ext4, the same host and cores, MB and seconds (`bench.py`, **read**):

| project | target before → after | the program before → after | cargo's target | edits before → after |
|---|---:|---:|---:|---|
| helix | 5,471 → 4,964 | 1,231 → 714 (rustc's 424) | 3,714 | body 47.1 → 48.5 (a full link both), signature 0.417 → 0.424, leaf 0.116 → 0.118, struct 0.298 → 0.299, new fn 0.135 → 0.134 |
| bevy | 23,487 → 21,930 | 3,657 → 2,083 (cargo's 2,016) | 17,551 | body 1.127 → 1.119, leaf 0.397 → 0.407, new fn 0.970 → 0.974 |

- On XFS the same: ripgrep 503 → 503 MB, helix 4,538 → 4,546, nushell 5,537 after (cargo's
  407, 3,713, 5,343), edits within a run's noise (**read**).
- Laying out bevy's 1.58 GB of debug sections takes 274-331 ms either way, once per daemon,
  off the edit; a splice's own moves took 16-34 µs on bevy's edits (**read**, the daemon log).
- After F/G with no reflinks, bevy is 21,930 less `.blaze-units` (4,219) and rustc's program
  kept (2,017): 15,694 MB against cargo's 17,551 (**unverified**, the same assumption as the
  table above).

## The daemon's program against rustc's

helix's `hx`, MB per section, the linked programs of the bench (**read**, their section
headers; `blaze.o` is the object the daemon compiled, packed between links).

| section | rustc's | the daemon's | of it from `blaze.o` |
|---|---:|---:|---:|
| `.debug_str` | 146.8 | 224.8 | 78.0 |
| `.debug_info` | 132.5 | 224.0 | 94.6 |
| `.text` | 37.1 | 82.2 | 36.3 |
| `.strtab` | 40.3 | 54.4 | 6.3 |
| `.debug_line` | 23.1 | 35.1 | 12.3 |
| `.debug_ranges` | 14.4 | 24.6 | 10.4 |
| `.symtab` | 4.9 | 13.8 | 14.8 in the object, most of it section symbols the link drops |
| `.eh_frame` | 4.5 | 10.9 | 5.0 |
| `.rela*` | 4.9 | 7.2 | |
| **the file** | **424** | **748** | |

- The daemon links `blaze.o` (151,608 functions Cranelift compiled) with rustc's whole link
  line, `--allow-multiple-definition`, so rustc's objects and their debug information are in
  the program beside blaze's: of the 324 MB over rustc's, blaze's own code, unwind tables
  and debug information are about 240, the rest rustc's linked twice (**read**). G builds
  every crate with blaze and links none of rustc's; ripgrep's program there is 50.1 MB
  against cargo's 53.2 (**reported**, [no-rustc.md](../no-rustc.md)). helix built that way fails
  at a build script today, so its program after G is not measured.
- Not padding: the 64 MiB of room behind the image is a hole until a splice writes code into
  it, and a splice writes its 10.5 MB of tables in place, the same blocks each time. XFS keeps
  blocks past the end of a file it just grew (245 MB seen right after a link) and lets them
  go later; a measure taken then counts them (**read**, FIEMAP's `unwritten,eof` extents).
- Strings duplicated across units: `object` writes `.debug_str` `SHF_MERGE|SHF_STRINGS` with
  an entry size of 0, which lld does not merge. Entry size 1 lets it: helix's `.debug_str`
  224.8 → 188.0 MB, the full link 1,269 → 1,605 ms (**read**, alternating runs, the same
  inputs). Not shipped: most of what it merges is rustc's copy beside blaze's, which G
  removes, and a third of a second on every full link is the edit loop's.
- Types are already one per unit of 256 functions (`hir-ty-codegen/debuginfo.rs`); symbols are
  what a debugger and a backtrace read, as in cargo's debug program.

## `blaze.o`

- It is the whole program as the daemon compiled it, written whenever the daemon links a
  program of its own rather than splicing into rustc's: ripgrep 35 MB, nushell 474 and helix
  571 in this bench, during the warm-up; bevy, whose first edits were spliced into rustc's
  program, has none (**read**, the files' times). A patch is laid out against it, and a full
  link links it (`main.rs` `sync`, `cargo/plan.rs`). Once F/G has rustc build no program,
  every project has one.
- helix's body edit links in full: "no patch: a closure in what rustc linked is another now",
  a 151k-function compile and 40 s against cargo's 2.3 s (**read**, its bench log), on main
  and on the branch.
- Only a link reads it on Linux: the splice of an ELF executable reads the executable and
  the patch, not the object (`splice.rs` `Baseline::parse`), and the debugger reads the
  executable. So the daemon keeps it zstd-packed beside where it was, `blaze.o.zst`, from the
  end of each link to the next that reads it, which inflates it there first (`whole.rs`).
  Not on macOS, where a debugger reads the object, nor with `split-debuginfo`, nor for the
  harness's batch builds, which read the object.
- Not in the global store: no other checkout or daemon has the same bytes, and the store's
  collection could drop it while the daemon still links with it (**unverified**).
- Packing takes 12-126 ms after a link, 16 threads at level 1 (**read**, the daemon logs);
  inflating happens only for a link of an object a patch was spliced over, which a full build
  does not need (it writes a new one).

## Compressed debug information

Compressed with Python over the ELF section headers (`SHF_COMPRESSED`), since neither
nixpkgs' binutils 2.46 `objcopy` nor LLVM 20's `llvm-objcopy` writes zstd (**read**).

| reader | zlib | zstd |
|---|---|---|
| std's backtrace (1.99.0) | file and line | file and line |
| gdb 17.2 | breakpoints, `bt`, locals | the same |
| lldb 20.1.8 | breakpoints, `bt`, locals | no line table: fails |
| binutils 2.46 `addr2line` | file and line | not built with zstd |

(**read**, a fixture and bevy's `breakout`, each reader run on each.)

- zlib is the one path every reader takes. On bevy's program: debug sections 1,576 → 376 MB,
  the file 2,021 → 768 MB; inflating them all takes 3.8 s on one core (**read**).
- What it costs: a panic's backtrace inflates every section, about 4-6 s and 1.6 GB of memory
  per symbolized panic on bevy (**unverified**, from the inflate time); gdb's start 11.47 →
  8.32 s, lldb's 3.49 → 6.41 s (**read**).
- Off by default, decided 2026-10-23: a panic's backtrace is part of the edit loop, and 4-6 s
  of inflating on each is worse there than 1.25 GB on disk. Split DWARF in the store would keep
  the program small without the panic's cost, but lldb and std read `.dwo` and `.dwp`
  differently, and it is not measured here.

## The store

- 64K blocks compress helix's store 4.2x at zstd 1 and 4.5x at 3; 256K blocks 5.4x and 5.9x;
  about 700-780 MB/s to compress, 1.4-1.6 GB/s to read back, one core (**read**,
  `packstat.py` on helix's 229,358 records). 128K at level 3 is what shipped: 5.0x on helix,
  5.1x on bevy.
- Its bound is a tenth of what the disk has for it (free space plus the store), at least
  256 MiB and at most 32 GiB, recomputed at each collection; `BLAZE_CACHE_MAX` sets bytes or
  `off`, `blaze-engine cache gc` collects, `blaze status` says the size and the bound
  ([cache.md](../cache.md)) (**read**).
- `bench.py` says of each project how much of its store no project before it in the run had
  stored (`store_added_mb`).
- A pack sent to a remote is `packs/<sha256>.pack` as the store has it, zstd blocks already;
  it was zstd again as `.pack.zst`, which an older blaze's remote may still hold and is still
  read ([cache.md](../cache.md)) (**read**).

## Left

- The daemon's program larger than rustc's (helix 748 MB against 424), rustc's objects linked
  beside blaze's until G; then measuring helix's, which fails at a build script today.
- helix's closure fallback, which links in full.
- Split DWARF in the store, if a program's debug information is to be smaller and a panic
  not to pay for it.

## First pass (2026-10-22 main, against #136)

Every block counted once, MB, after the bench's edits.

| project | blaze target | store before | store after | blaze total before → after | cargo target |
|---|---:|---:|---:|---:|---:|
| ripgrep | 532 | 20 | 6 | 552 → 538 | 407 |
| helix | 4,774 | 1,686 | 339 | 6,462 → 5,113 | 3,713 |
| nushell | 5,927 | 219 | 64 | 6,159 → 5,991 | 5,343 |
| bevy | 31,575 | 8,334 | 1,623 | 39,919 → 33,198 | 17,549 |

bevy's 31,575 is the run whose edits fell to full links (above). Counted by `du`, as
`bench.py` did before #131, it was 43,063 MB: reflinked copies counted twice.
