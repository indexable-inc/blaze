---
status: plan
summary: A dev build's executable carries no debug info - it is a side file every debugger and std's backtrace find unaided (ELF `<name>.debug` by `.gnu_debuglink` with a CRC that is a function of the build id, Mach-O `<name>.dSYM` by `LC_UUID`), kept as per-function records patched in place; one program is one link however many paths it is placed at. Built - the side file, its model and writer, updates that encode only a link's changed functions, the profile default, one link per unhashed program, a build id of what the image holds and not of its directory, `link write` events by region. Not built - the write moved after the answer, the side file as a store output, the kept image keyed by inputs, the writers' tables by entry, the maps as records, the patch room out of the file (patch-in-place.md). The 162 MB is mostly not debug info - at least 128 MiB is the two patch regions' room.
date: 2026-11-10
---
# Debug info beside the executable

Hello world's dev executable is 162,021,720 bytes, written, hashed and placed every edit, and
linked twice. Follows [item-hash.md](item-hash.md) section 13, [patch-in-place.md](patch-in-place.md).

## What the 162 MB is (from the code; the split needs measuring)

The debug info the linker makes is a few megabytes: one DWARF 4 unit of line rows and a
15-byte subprogram a function (`dwarf.rs`), no types, no variables; `.debug_str` holds the
mangled names (1 to 3 MB, which `.strtab` holds again). **At least 134,217,728 of the
162,021,720 bytes (82.8%) are the two patch regions' room** (64 MiB each, in the file up to
its end, `elf/image.rs` `tail`); the other 27.8 MB is code, data, unwind tables, each growing
region's doubled room, the symbol table and the debug info. Measure with `readelf -SW` /
`otool -l`, `du` against `ls -l`. The room out of the file: [patch-in-place.md](patch-in-place.md).

## Where debug info goes

`blaze_link::Debug::Beside`: the image carries no `.debug_*`; `Image::debug_file()` gives the
side file. `codegen::debug_place`: `split-debuginfo` unset, `packed` or `unpacked` is beside;
`"off"` is `Debug::Lines`, the layout as before.

| Platform | Executable keeps | Side file | Found by |
|---|---|---|---|
| ELF | code, data, `.eh_frame`, `.symtab`, `.note.gnu.build-id`, a `.gnu_debuglink` (name, CRC-32) | `<name>.debug` beside it: the executable's sections with no bytes, its build id note, DWARF, a copy of `.symtab`/`.strtab` | `.gnu_debuglink` in the executable's directory |
| Mach-O | the same, `LC_UUID` | `<name>.dSYM/Contents/Resources/DWARF/<name>`: `LC_UUID`, the segments with no bytes, `__DWARF`, the symbol table | the bundle beside it, by UUID |
| PE, wasm | as before (wasm: the name section) | none: blaze writes no PDB | nothing changed |

How sure, with nothing configured (none of it run here; `harness/v2/debug_side.py` checks):

- **std's backtrace, Linux**: sure. `backtrace-rs` `gimli/elf.rs` tries the build id under
  `/usr/lib/debug/.build-id` (absent), then `.gnu_debuglink` in the executable's directory,
  with no CRC check, and then takes symbols from the side file, so it copies `.symtab`.
- **std's backtrace, macOS**: sure. `gimli/macho.rs` scans the directory for a `*.dSYM` of its UUID.
- **gdb**: fairly sure. It finds `<dir>/<debuglink name>` and checks the CRC; the side file has
  the executable's program headers with nothing in the file but the build id note and its
  sections as `NOBITS`, as `objcopy --only-keep-debug` leaves them.
- **lldb**: fairly sure. Linux as gdb; macOS `<exe>.dSYM` beside the executable by UUID, with
  the `Info.plist` dsymutil writes. No `.build-id/` directory: that needs a configured root.

## The CRC is of the build id

`.gnu_debuglink` holds the side file's CRC-32, and gdb refuses a file whose CRC differs. A CRC
of the side file's bytes would make the executable wait for its debug info. So the executable
holds `link_crc(id)`, the build id's first four bytes, and the side file's last four bytes are
chosen to make its CRC that (`crc_fix`): a side file of another generation fails gdb's check.

## The side file is records (`crates/blaze-link/src/beside.rs`)

- `.debug_info`: the unit, then one 17-byte subprogram a function at a fixed slot (name offset,
  address, size). A slot whose function is gone is an empty range and is reused.
- `.debug_line`: one program; a function's rows are its own sequence, in any order. A changed
  function's sequence is appended and the old one overwritten with `DW_LNS_advance_pc 0`
  (`dwarf::dead`). `.debug_str`: names, appended. The symbol table: copied, by 4 KiB block.
- Every section has room (twice its size). Laid out whole, saying why (`DebugWrite::whole`):
  the first write, no room, a source file the line header does not name, more than half of
  `.debug_line` dead, a file on the disk that is not the last one written.

`Kept::update` holds a hash a function (address, size, name, encoded rows) and writes the
records whose hash changed: a body edit two (the function made and the one replaced), a line
edit above the functions of that file after it. It follows the link's change: a stage leaves
on its state the keys it added, removed or named and the span tables it gave (`beside::Delta`),
and `Image::debug_file` encodes those functions alone when the side file is of the generation
staged from (`DebugWrite::encoded`); a span table given anew costs a pass comparing each line's
anchor. Otherwise (first write, full layout, rollback, a skipped update, records that do not
number the image's functions) every function is encoded, and `DebugWrite::all` says why. Still
of the program each update: a count of the pieces, the symbol table compared by block. A patch
names the build id of the file it is over (`DebugWrite::base`); `apply` refuses any other.

**Between the answer and the write** a debugger sees the previous side file: on ELF gdb refuses
it by CRC and std reads it (the edited function's lines stale); on macOS both refuse it by UUID
and use symbols. It is replaced by an unsynced write of a clone and a rename.

## One link for one program

Two images existed because `artifact::made` listed both `build/<pkg>/<hash>/out/<name>` and
`<profile>/<name>`: `rustc_writes` took every output of a unit with no `extra_filename` (cargo's
new layout) as rustc's own, so `uplifted` found no copy. Now an output outside the unit's output
directory with a same-named output inside is its copy (`copy_of_unhashed`): one made file, one
kept image, one link; the profile directory's is a placement of the same key (`compile.rs`
finds it by `uplifted`), the dev run held under both by `runs_from`. The kept image is still
found by its made path: it is what a link patches; what it holds is `Rows::given`'s to check.

The build id was of the path: `lineage` hashed `to`. Only the file name is in the file (a Mach-O
signature's identifier, an import library's DLL, the `.gnu_debuglink`); the directory is not
(`DW_AT_comp_dir` is `.`; rpaths are the output's own field, hashed). `lineage` now hashes the
name alone, so the same program for two directories is the same bytes. Two checkouts' copies
of one program then share a lineage. No live patch reaches the wrong process for it: a patch
goes to the processes mapping its image's code memory (`inproc::Loaded`); the message's id
only refuses another image's. But each copy counts its own generations, so the build id is
now of lineage, generation and what the image holds (`Change::given`, `Rows::given`).

## Not built, and why

- **The write after the answer.** `codegen::debug_side` runs in the link, in the span
  `link debug file` (`bytes written ranges records encoded all whole`). The hook is a `Steps`
  method that takes the kept image after the answer, calls `Image::debug_file()` and writes
  as `write_side` does, its failure a `Settling` loss. Not written blind: `job.rs` is a
  simulated state machine generic over `Steps`; a new task kind there needs its tests run.
- **The side file in the store**, so a build that places the executable without linking
  places it too (today it writes none: no lines in backtraces until the next link). It fits
  `Written::Derived`: a `digest::Digest` kept by its writes, an output beside each path.
- **The kept image keyed by the link's inputs**: still found by its made path, which the
  loop's kept-image weights index by (`job.rs`, `work::Image::path`).
- **The writers' tables by entry** (`file.rs`). First the measure: a stage now says what it
  writes by region, part and debug section (`link write` events, `shape::record_writes`).
  **Address and perf maps as records** (`layout.rs`, `perf.rs`). **PDB**.

## Tests

- `beside/tests.rs`: the refinement `a_side_file_kept_by_patches_resolves_as_debug_info_inside`
  (random edits, each update given the link's change or every function, against
  `dwarf::sections` of the same program read with gimli; build id, symbols, CRC); controls
  `the_oracle_catches_a_side_file_of_another_build_id`,
  `..._a_line_row_left_for_a_moved_function`, `..._a_debuglink_crc_not_updated`,
  `..._a_changed_function_left_out_of_the_change`; counter
  `a_one_function_edit_rewrites_its_own_records` (records written and functions encoded, at
  100 and 10,000 functions); `a_change_that_misses_a_removed_function_is_made_from_every_function`.
- `tests/beside.rs`: `an_image_and_its_side_file_resolve_as_debug_info_inside` (first link,
  edit, rollback, a side file gone; program headers); the counter
  `the_executable_has_no_section_that_grows_with_debug_info`; `debug_info_inside_is_the_same_file_twice`;
  `an_images_build_id_is_of_the_program_not_of_its_directory` (and of what it holds).
- blaze-cargo: `debug_info_goes_beside_the_image_unless_the_profile_says_off`,
  `a_program_with_no_hash_in_its_name_is_made_once`. `harness/v2/debug_side.py`: hello world
  both ways: sizes, build id and CRC, the same backtrace, gdb and lldb or "not checked: no gdb".
