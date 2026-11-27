---
status: plan
summary: The debug info of an image `blaze build` exports goes in a side file every debugger and std's backtrace find unaided (ELF `<name>.debug` by `.gnu_debuglink` with a CRC that is a function of the build id, Mach-O `<name>.dSYM` by `LC_UUID`), kept as per-function records so an edit rewrites only its functions' records; a build id of what the image holds, not of its directory. Not built: crates/blaze-export writes no debug info today. A program `run` places is described to a debugger by the GDB JIT interface (crates/blaze-launch-rt/src/jit_debug.rs) instead.
date: 2026-11-10
---
# Debug info beside the executable

An image `blaze build` exports keeps its code, data, unwind tables, symbol table and build id;
its debug info is a side file beside it. Nothing of this is built: crates/blaze-export writes no
`.debug_*` section and no side file. Code `run` and `test` place in a running process is
registered with a debugger through the GDB JIT interface (`crates/blaze-launch-rt/src/jit_debug.rs`)
and needs no side file. Keys are [bottom.md](bottom.md) section 3's.

## Where debug info goes

`split-debuginfo` unset, `packed` or `unpacked` puts it beside the image; `"off"` puts the line
tables inside.

| Platform | Executable keeps | Side file | Found by |
|---|---|---|---|
| ELF | code, data, `.eh_frame`, `.symtab`, `.note.gnu.build-id`, a `.gnu_debuglink` (name, CRC-32) | `<name>.debug` beside it: the executable's sections with no bytes, its build id note, DWARF, a copy of `.symtab`/`.strtab` | `.gnu_debuglink` in the executable's directory |
| Mach-O | the same, `LC_UUID` | `<name>.dSYM/Contents/Resources/DWARF/<name>`: `LC_UUID`, the segments with no bytes, `__DWARF`, the symbol table | the bundle beside it, by UUID |
| PE, wasm | wasm: the name section | none: blaze writes no PDB | nothing |

How sure, with nothing configured (none of it run):

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
chosen to make its CRC that: a side file of another generation fails gdb's check.

## The side file is records

- `.debug_info`: the unit, then one 17-byte subprogram a function at a fixed slot (name offset,
  address, size). A slot whose function is gone is an empty range and is reused.
- `.debug_line`: one program; a function's rows are its own sequence, in any order. A changed
  function's sequence is appended and the old one overwritten with `DW_LNS_advance_pc 0`.
  `.debug_str`: names, appended. The symbol table: copied, by 4 KiB block.
- Every section has room (twice its size). It is laid out whole, saying why, on the first write,
  with no room, for a source file the line header does not name, with more than half of
  `.debug_line` dead, or over a file on the disk that is not the last one written.

A hash a function (address, size, name, encoded rows) picks the records to write: a body edit
writes two (the function made and the one replaced), a line edit those of the functions of that
file after it. A patch names the build id of the file it is over and is refused over any other.
The ELF seal (the CRC-32 of the whole file) is kept by 4 KiB block (a tree of sums, CRC being
linear), so a patch reads the blocks it wrote, not the file.

Between the answer and the write a debugger sees the previous side file: on ELF gdb refuses it
by CRC and std reads it (the edited function's lines stale); on macOS both refuse it by UUID and
use symbols. It is replaced by an unsynced write of a new file and a rename.

## One image for one program

The build id is of what the image holds, not of its directory: only the file name is in the
file (a Mach-O signature's identifier, an import library's DLL, the `.gnu_debuglink`;
`DW_AT_comp_dir` is `.`), so the same program for two directories is the same bytes, and a
program placed at two paths is one image, written once a path.

## Open

- The side file as a store output, so placing an image without exporting it again places its
  debug info too.
- Address and perf maps as records. PDB.
- The test: hello world both ways (sizes, build id and CRC, the same backtrace, gdb and lldb or
  "not checked: no gdb").
