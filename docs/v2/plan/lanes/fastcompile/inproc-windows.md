---
status: plan
summary: blaze v2 fastcompile, the Windows x86-64 half of in-process runs (crates/blaze-inproc sys/windows.rs) - the program started from its own executable, its image region rebuilt by the daemon before main so the code ranges are views of a pagefile-backed section the daemon writes (placeholders, every other page put back as it was), a named pipe for this user only, and what blaze-link must lay out.
date: 2026-10-29
---
# fastcompile-inproc-windows: a running image laid over by a shared section

The Windows x86-64 platform layer of [inproc.md](inproc.md): `crates/blaze-inproc/src/sys/windows.rs`
and `sys/windows/pipe.rs`. The program is its own executable, started by the kernel; its start
hook (a TLS callback, so before the CRT's start) asks the daemon to lay the code memory over its
image. The PE loader that used to run programs (`load/pe.rs`) is gone.

## State of the art matched

- **Dual mapping:** .NET 7's W^X (`ExecutableAllocator`) maps one pagefile-backed section twice: a
  writable view for the JIT and an executable view for running. blaze does the same across processes.
  The daemon holds the writable view; the run process gets executable views of the same section.
  The daemon maps those views into the run process itself with `MapViewOfFile3` on a process handle.
  The run process never receives a handle to the section.
- **No race between reserve and map:** placeholders (`VirtualAlloc2` with `MEM_RESERVE_PLACEHOLDER`,
  split with `VirtualFree(MEM_PRESERVE_PLACEHOLDER)`, replaced with `MEM_REPLACE_PLACEHOLDER`) are
  Microsoft's documented way to do this. Chromium and Wine's `ntdll` use them to place views
  exactly. Nothing else can take the range between reserving it and mapping it.
- **The system loader does the loading.** Imports, relocations, TLS, `.pdata`, the CRT start and
  the command line are the NT loader's, as in a whole build: `GetModuleHandle(NULL)`,
  `GetModuleFileName`, `current_exe`, resources, `RtlPcToFileHeader` (so C++ exceptions and Rust
  panics) and stack walks all see the program's own module.

## Memory

- **Memory object:** `CreateFileMappingW(INVALID_HANDLE_VALUE, PAGE_EXECUTE_READWRITE | SEC_COMMIT)`.
  The whole object is charged against the commit limit when created, so no write can fault for
  lack of commit later. The daemon maps it read-write here.
- **Granularity:** every view starts and ends on the 64 KiB allocation granularity, which is what
  `sys::page()` returns. All offsets, lengths and addresses of the overlays are multiples of it.
- **Objects:** after the GRANT, a second GRANT frame at sequence number 1 lists each object's
  length (u64) and rights (u8). The daemon keeps the handles.
- **MAP frames:** the hook maps a whole object anywhere (the control words and the asks, shared)
  by asking the daemon with MAP (object u32, access u8). The daemon checks the object's rights
  (never executable anywhere, the image never shared writable) and answers MAPPED with the
  address, or REFUSED with why.
- **Asks** (`ask.rs`): Windows has no `WaitOnAddress` across processes, so right after the
  objects the daemon makes 65 auto-reset events, the doorbell and one per slot, and duplicates
  each into the run process (`DuplicateHandle`, so the process handle is opened with
  `PROCESS_DUP_HANDLE` and `PROCESS_SYNCHRONIZE`); it writes their handle values into the asks
  object's header before it answers any MAP. An asker sets the doorbell and waits on its slot's
  event; the daemon's reader waits on the doorbell and the process handle at once, so a process
  that ended ends it. A thread waiting for a free slot sleeps with `WaitOnAddress` in its own
  process. `Session::kill` is `TerminateProcess`, and a process already gone counts as killed.
- **Channel:** a named pipe `\\.\pipe\blaze-inproc-<pid>-<random>`, with remote clients rejected.
  - The DACL gives the user everything and gives ALL APPLICATION PACKAGES connect rights only, so
    a program in the `blaze.sandbox` AppContainer can connect.
  - A low mandatory label with NO_WRITE_UP lets the low-integrity sandbox in.
  - On every accept, the peer's process is looked up with `GetNamedPipeClientProcessId`, and its
    token user must be ours.
  - The client opens the pipe at SecurityIdentification, so the server can't impersonate it.
  - A read that times out cancels the I/O but still returns a transfer that completed meanwhile.
- **Other functions:**
  - `threads`: a Toolhelp32 walk. A walk that fails partway is an error, never a shorter list,
    because the fence reaps the slots of threads that aren't listed.
  - `backtrace`: a walk with `RtlLookupFunctionEntry` and `RtlVirtualUnwind`, through the
    program's `.pdata` as the system loader registered it. It returns `None` at a frame with no
    function entry or when the buffer is full.
  - `publish`: `FlushInstructionCache`.
  - `random`: `ProcessPrng`.
  - `own_image`: `GetModuleHandleW(NULL)` and the headers' `SizeOfHeaders` bytes there.
  - `clone_file`: none (NTFS has no clones; ReFS block cloning is not asked for), so the
    executable is written.

## The rebuild (`Remote::overlay`)

The image is one view of an image section, which can't be partly unmapped or replaced. When the
hook sends OVERLAY (after checking the grant against its own headers, as on every platform), the
daemon, holding the process with `PROCESS_VM_OPERATION | VM_READ | VM_WRITE |
QUERY_INFORMATION | TERMINATE`:

1. checks every overlay: granular, its object's rights allow its access;
2. walks the image's span with `VirtualQueryEx`: every committed region must be of the image's
   own allocation (`MEM_IMAGE`, allocation base the header);
3. reads every committed page outside the overlays with `ReadProcessMemory` (`Kept`), refusing
   any that is executable or a guard page outside them (code runs only from the code memory)
   and turning `PAGE_WRITECOPY` into `PAGE_READWRITE` (a private page is what a written
   copy-on-write page becomes anyway);
4. unmaps the image (`UnmapViewOfFile2`) and reserves its span, rounded to the granule, as one
   placeholder (`VirtualAlloc2` `MEM_RESERVE_PLACEHOLDER`);
5. splits it at the overlays (`VirtualFreeEx` `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`) and fills
   each piece: an overlay's view (`MapViewOfFile3` `MEM_REPLACE_PLACEHOLDER`), or reserved private
   memory (`VirtualAlloc2` `MEM_RESERVE | MEM_REPLACE_PLACEHOLDER`);
6. commits the kept pages, writes their bytes back (`WriteProcessMemory`) and gives them their
   protection (`VirtualProtectEx`);
7. answers OVERLAID.

Every byte at every address is what it was. The hook's thread is the only one running the
program (TLS callbacks at process attach come before any thread the program makes) and waits in
`ReadFile` on the pipe, on its own stack and a heap buffer, so it touches no page of the image
until OVERLAID. A failure after step 4 ends the process with `TerminateProcess(127)`: an image
half replaced must not run. The planning (`pieces`, `outside`, `private_protection`) is pure and
unit-tested.

## What blaze-link must do

- Put the code sections together, starting on a 64 KiB RVA, and start the first section after
  them on the next 64 KiB RVA: views can't split a granule, so a granule holding both code and
  data would need a view both writable and executable.
- Keep `PointerToRawData == VirtualAddress` for code, so a file offset is an object offset.
- Write no base relocation into code (address RIP-relative only): the NT loader would write the
  file's code pages, which then differ from the object's at the swap.
- Disable CFG for dev runs: the loader's CFG bitmap describes the image section's pages, and the
  rebuilt region is a view the bitmap does not cover.
- Put the TLS callback pointing at `blaze_inproc_start` first in `.CRT$XLA*` (`.CRT$XLAB`).
- Carry reservations for the arena, cells and fixed maps: sections with no access and no raw data.

## Known differences from a whole build

- Software breakpoints in the code ranges can't be written (`WriteProcessMemory` into a
  read-execute view fails); hardware breakpoints work.
- `VirtualQuery` of the image's addresses says `MEM_MAPPED` (code) and `MEM_PRIVATE` (the rest)
  where it said `MEM_IMAGE`.
