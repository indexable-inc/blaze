//! Placed code made visible to a debugger and to a backtrace (docs/v2/plan/design/backend-next.md,
//! the critic's point 10): the GDB JIT interface, an in-memory object a placed batch, and the
//! position map read back into an inline chain.
//!
//! **The JIT interface** (GDB's manual, "JIT Compilation Interface"; LLDB's `JITLoaderGDB`
//! reads the same): the process defines `__jit_debug_descriptor` and a function
//! `__jit_debug_register_code` the debugger stops at; to tell it of new code, the process links an
//! entry naming an object file's bytes into the descriptor's list, sets `relevant_entry` and
//! `action_flag`, and calls the function. [`Jit`] is that, for one thread (`!Send`); a
//! [`Registered`] batch unregisters itself when dropped, and borrows the [`Jit`], so no entry
//! outlives the one writer. LLDB on macOS reads the interface only after `settings set
//! plugin.jit-loader.gdb.enable on` (its default is off on Darwin).
//!
//! **The object** ([`object`]) is ELF64 whatever the host: an `ET_EXEC` with no program
//! headers, its `.text` `NOBITS` at the batch's run address (the code is in memory already; the
//! object only names it), a symbol a placed function, and DWARF 4 line tables made from each
//! record's position map: one compile unit a function, its lines the innermost frame's. ELF on
//! macOS: GDB and LLDB both read an ELF symbol file through this interface (LLDB's
//! `ObjectFile` plugins take either); TO VERIFY with lldb on macOS that `image list` shows the
//! batch and `bt` names a placed function.
//!
//! **Positions are item-relative** (rule 71): a position row says a span in an item's text. The
//! object is where they are printed, so this is the one place they become a file, a line and a
//! column, through the caller's [`Lines`] (which knows each item's file and where it starts).
//! A backtrace reads the record's rows directly ([`chain`]): the innermost body first, each
//! spliced body named by its instance, then each call site it was spliced at, out to the
//! function's own body.

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use blaze_code::eh::{sleb, uleb};
use blaze_own::{AVec, Alloc, Box, Vec};
use blaze_run::InstanceKey;
use blaze_tier::{Arch, InlineFrame, OUTERMOST, Position};

/// One placed function of a batch, as its record says.
#[derive(Clone, Copy)]
pub struct Code<'r> {
    /// Its first byte's run address.
    pub at: u64,
    pub len: u32,
    /// The function's own instance (the body of frame [`OUTERMOST`]).
    pub instance: InstanceKey,
    /// Its symbol's name (no NUL).
    pub name: &'r [u8],
    /// The record's position map, sorted by `at`.
    pub positions: &'r [Position],
    /// The record's inline frames, parents before children.
    pub frames: &'r [InlineFrame],
}

/// A file, a line and a column, each as a debugger counts them (lines and columns from 1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Line {
    /// The caller's file id ([`Lines::file`]).
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// The caller's knowledge of item texts: where an item-relative offset is when printed.
pub trait Lines {
    /// Where byte `at` of `instance`'s item text is; `None` when the caller has no text for it
    /// (the object refuses the record by name).
    fn locate(&self, instance: InstanceKey, at: u32) -> Option<Line>;
    /// File `file`'s path as a debugger shows it (no NUL).
    fn file(&self, file: u32) -> Option<&[u8]>;
}

/// One frame of a pc's inline chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Located {
    /// The body the code is of.
    pub instance: InstanceKey,
    /// A span in that instance's item text: the code's own for the innermost, a call site for
    /// each one out.
    pub span_lo: u32,
    pub span_hi: u32,
    /// Whether this body was spliced into another (every one but the last).
    pub inlined: bool,
}

/// Why no object or chain was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JitError {
    /// The batch is not sorted by address, or two functions overlap.
    Unsorted { at: u64 },
    /// A function has no code.
    Empty { at: u64 },
    /// A name holds a NUL byte.
    NameNul { at: u64 },
    /// A position row names a frame past the frames.
    FrameIndex { at: u64, frame: u32 },
    /// An inline frame's parent is not before it.
    ParentAfterChild { at: u64, frame: u32, parent: u32 },
    /// A position row is unsorted or past the function's code.
    Position { at: u64, offset: u32 },
    /// The pc is before the function's first position row.
    NoPosition { at: u64, offset: u32 },
    /// The caller has no text for an instance a row names.
    NoLine { instance: InstanceKey, offset: u32 },
    /// The caller has no path for a file it gave.
    NoFile { file: u32 },
    /// A size is past what its field holds.
    TooLarge { what: &'static str },
    /// The descriptor is claimed already (one writer a process).
    Taken,
}

impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-launch-rt jit_debug.rs)";
        match *self {
            JitError::Unsorted { at } => write!(f, "the placed batch is unsorted or overlaps at {at:#x} {rule}"),
            JitError::Empty { at } => write!(f, "the placed function at {at:#x} has no code {rule}"),
            JitError::NameNul { at } => write!(f, "the placed function at {at:#x} has a name with a NUL byte {rule}"),
            JitError::FrameIndex { at, frame } => write!(f, "the function at {at:#x} has a position in inline frame {frame}, past its frames {rule}"),
            JitError::ParentAfterChild { at, frame, parent } => {
                write!(f, "the function at {at:#x} has inline frame {frame} spliced into frame {parent}, which is not before it {rule}")
            }
            JitError::Position { at, offset } => write!(f, "the function at {at:#x} has a position row at {offset} out of order or past its code {rule}"),
            JitError::NoPosition { at, offset } => write!(f, "the function at {at:#x} has no position row at or before offset {offset} {rule}"),
            JitError::NoLine { instance, offset } => {
                write!(f, "no item text is known for instance {:02x?}.. (offset {offset}): its lines cannot be printed (rule 71) {rule}", &instance.0[..4])
            }
            JitError::NoFile { file } => write!(f, "no path is known for file {file} {rule}"),
            JitError::TooLarge { what } => write!(f, "the debugger object's {what} is past what its field holds {rule}"),
            JitError::Taken => write!(f, "the JIT debug descriptor is claimed already: the control thread is its one writer {rule}"),
        }
    }
}

impl core::error::Error for JitError {}

/// The body frame `frame` is of in `code`.
fn instance_of(code: &Code<'_>, frame: u32) -> Result<InstanceKey, JitError> {
    match frame {
        OUTERMOST => Ok(code.instance),
        k => code.frames.get(k as usize).map(|f| f.instance).ok_or(JitError::FrameIndex { at: code.at, frame: k }),
    }
}

/// Appends to `out` the inline chain at code offset `offset` of `code`, innermost first: the
/// body the code is of with its span, then for each splice out, the body it was spliced into
/// with the call site's span; the last is the function's own body. A work list over the parent
/// links, each strictly before its child, so at most `frames.len() + 1` entries.
pub fn chain(code: &Code<'_>, offset: u32, out: &mut AVec<'_, Located>) -> Result<(), JitError> {
    let row = code.positions.partition_point(|p| p.at <= offset);
    let Some(position) = row.checked_sub(1).and_then(|i| code.positions.get(i)) else {
        return Err(JitError::NoPosition { at: code.at, offset });
    };
    let mut frame = position.frame;
    out.push(Located { instance: instance_of(code, frame)?, span_lo: position.span_lo, span_hi: position.span_hi, inlined: frame != OUTERMOST });
    while frame != OUTERMOST {
        let this = *code.frames.get(frame as usize).ok_or(JitError::FrameIndex { at: code.at, frame })?;
        if this.parent != OUTERMOST && this.parent >= frame {
            return Err(JitError::ParentAfterChild { at: code.at, frame, parent: this.parent });
        }
        out.push(Located { instance: instance_of(code, this.parent)?, span_lo: this.call_lo, span_hi: this.call_hi, inlined: this.parent != OUTERMOST });
        frame = this.parent;
    }
    Ok(())
}

/// ELF and DWARF constants used (the System V ABI's ELF-64 object file format; DWARF 4).
mod c {
    pub const ET_EXEC: u16 = 2;
    pub const EM_AARCH64: u16 = 183;
    pub const EM_X86_64: u16 = 62;
    pub const SHT_PROGBITS: u32 = 1;
    pub const SHT_SYMTAB: u32 = 2;
    pub const SHT_STRTAB: u32 = 3;
    pub const SHT_NOBITS: u32 = 8;
    pub const SHF_ALLOC: u64 = 2;
    pub const SHF_EXECINSTR: u64 = 4;
    /// `STB_GLOBAL << 4 | STT_FUNC`.
    pub const SYM_FUNC: u8 = 0x12;
    pub const EHDR: usize = 64;
    pub const SHDR: usize = 64;
    pub const SYM: usize = 24;
    pub const DW_LANG_RUST: u16 = 0x1c;
    pub const DW_LNS_COPY: u8 = 1;
    pub const DW_LNS_ADVANCE_PC: u8 = 2;
    pub const DW_LNS_ADVANCE_LINE: u8 = 3;
    pub const DW_LNS_SET_FILE: u8 = 4;
    pub const DW_LNS_SET_COLUMN: u8 = 5;
    pub const DW_LNE_END_SEQUENCE: u8 = 1;
    pub const DW_LNE_SET_ADDRESS: u8 = 2;
    /// `opcode_base` 13: the twelve standard opcodes' operand counts.
    pub const STANDARD_LENGTHS: [u8; 12] = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];
    /// The one abbreviation: code 1, `DW_TAG_compile_unit`, no children, `DW_AT_name`
    /// string, `DW_AT_stmt_list` sec_offset, `DW_AT_low_pc` addr, `DW_AT_high_pc` data8,
    /// `DW_AT_language` data2.
    pub const ABBREV: [u8; 16] = [1, 0x11, 0, 0x03, 0x08, 0x10, 0x17, 0x11, 0x01, 0x12, 0x07, 0x13, 0x05, 0, 0, 0];
    /// The section names, in section order from 1, and the string table they are in.
    pub const SHSTRTAB: &[u8] = b"\0.text\0.symtab\0.strtab\0.debug_abbrev\0.debug_info\0.debug_line\0.shstrtab\0";
    pub const NAME_AT: [u32; 8] = [0, 1, 7, 15, 23, 37, 49, 61];
}

fn u16le(out: &mut AVec<'_, u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u32le(out: &mut AVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u64le(out: &mut AVec<'_, u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn size32(len: usize, what: &'static str) -> Result<u32, JitError> {
    u32::try_from(len).map_err(|_| JitError::TooLarge { what })
}

/// Writes the 4-byte length of the run starting at `at` (the length field itself) up to the end.
fn patch_len(out: &mut AVec<'_, u8>, at: usize, what: &'static str) -> Result<(), JitError> {
    let len = size32(out.len() - at - 4, what)?;
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Checks the batch: sorted, disjoint, non-empty, names without NUL, positions sorted and
/// inside each function.
fn check(batch: &[Code<'_>]) -> Result<(), JitError> {
    let mut end = 0u64;
    for code in batch {
        if code.len == 0 {
            return Err(JitError::Empty { at: code.at });
        }
        if code.at < end {
            return Err(JitError::Unsorted { at: code.at });
        }
        end = code.at.checked_add(u64::from(code.len)).ok_or(JitError::Unsorted { at: code.at })?;
        if code.name.contains(&0) {
            return Err(JitError::NameNul { at: code.at });
        }
        let mut last = 0u32;
        for position in code.positions {
            if position.at < last || position.at >= code.len {
                return Err(JitError::Position { at: code.at, offset: position.at });
            }
            last = position.at;
        }
    }
    Ok(())
}

/// The batch's debugger object: ELF64 for `arch`, a symbol and a compile unit with its line
/// table a function (the module says the form). `batch` is sorted by address. Deterministic: the
/// same batch and lines give the same bytes.
pub fn object<'a>(batch: &[Code<'_>], arch: Arch, lines: &impl Lines, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, JitError> {
    check(batch)?;
    let mut strtab: AVec<'a, u8> = Vec::new_in(alloc);
    strtab.push(0);
    let mut symtab: AVec<'a, u8> = Vec::new_in(alloc);
    symtab.extend_from_slice(&[0; c::SYM]);
    let mut info: AVec<'a, u8> = Vec::new_in(alloc);
    let mut line: AVec<'a, u8> = Vec::new_in(alloc);
    let mut located: AVec<'a, (u32, Line)> = Vec::new_in(alloc);
    let mut files: AVec<'a, u32> = Vec::new_in(alloc);
    for code in batch {
        // The symbol.
        let name_at = size32(strtab.len(), "string table")?;
        strtab.extend_from_slice(code.name);
        strtab.push(0);
        u32le(&mut symtab, name_at);
        symtab.push(c::SYM_FUNC);
        symtab.push(0);
        u16le(&mut symtab, 1);
        u64le(&mut symtab, code.at);
        u64le(&mut symtab, u64::from(code.len));

        // Every row printed once, here (rule 71), and the files they name, sorted (rule 25).
        located.clear();
        files.clear();
        for position in code.positions {
            let instance = instance_of(code, position.frame)?;
            let at = lines.locate(instance, position.span_lo).ok_or(JitError::NoLine { instance, offset: position.span_lo })?;
            located.push((position.at, at));
            files.push(at.file);
        }
        files.sort_unstable();
        files.dedup();

        // The line table: DWARF 4 section 6.2.4's header, then the rows with standard opcodes.
        let unit = line.len();
        let stmt_list = size32(unit, ".debug_line")?;
        u32le(&mut line, 0);
        u16le(&mut line, 4);
        let header_len_at = line.len();
        u32le(&mut line, 0);
        line.extend_from_slice(&[1, 1, 1]);
        line.push((-5i8) as u8);
        line.push(14);
        line.push(13);
        line.extend_from_slice(&c::STANDARD_LENGTHS);
        line.push(0);
        for &file in files.iter() {
            let path = lines.file(file).ok_or(JitError::NoFile { file })?;
            line.extend_from_slice(path);
            line.extend_from_slice(&[0, 0, 0, 0]);
        }
        line.push(0);
        let header_len = size32(line.len() - header_len_at - 4, ".debug_line header")?;
        line[header_len_at..header_len_at + 4].copy_from_slice(&header_len.to_le_bytes());
        line.extend_from_slice(&[0, 9, c::DW_LNE_SET_ADDRESS]);
        u64le(&mut line, code.at);
        let (mut pc, mut file, mut row, mut column) = (0u32, 1u64, 1i64, 0u32);
        for &(at, place) in located.iter() {
            if at > pc {
                line.push(c::DW_LNS_ADVANCE_PC);
                uleb(&mut line, u64::from(at - pc));
                pc = at;
            }
            let index = files.binary_search(&place.file).map_err(|_| JitError::NoFile { file: place.file })? as u64 + 1;
            if index != file {
                line.push(c::DW_LNS_SET_FILE);
                uleb(&mut line, index);
                file = index;
            }
            if i64::from(place.line) != row {
                line.push(c::DW_LNS_ADVANCE_LINE);
                sleb(&mut line, i64::from(place.line) - row);
                row = i64::from(place.line);
            }
            if place.column != column {
                line.push(c::DW_LNS_SET_COLUMN);
                uleb(&mut line, u64::from(place.column));
                column = place.column;
            }
            line.push(c::DW_LNS_COPY);
        }
        if code.len > pc {
            line.push(c::DW_LNS_ADVANCE_PC);
            uleb(&mut line, u64::from(code.len - pc));
        }
        line.extend_from_slice(&[0, 1, c::DW_LNE_END_SEQUENCE]);
        patch_len(&mut line, unit, ".debug_line unit")?;

        // The compile unit.
        let cu = info.len();
        u32le(&mut info, 0);
        u16le(&mut info, 4);
        u32le(&mut info, 0);
        info.push(8);
        uleb(&mut info, 1);
        info.extend_from_slice(code.name);
        info.push(0);
        u32le(&mut info, stmt_list);
        u64le(&mut info, code.at);
        u64le(&mut info, u64::from(code.len));
        u16le(&mut info, c::DW_LANG_RUST);
        patch_len(&mut info, cu, ".debug_info unit")?;
    }

    // Layout: the header, then each section's bytes (8-aligned), then the section headers.
    let parts: [&[u8]; 6] = [symtab.as_slice(), strtab.as_slice(), &c::ABBREV, info.as_slice(), line.as_slice(), c::SHSTRTAB];
    let mut offsets = [0u64; 6];
    let mut at = c::EHDR;
    for (i, part) in parts.iter().enumerate() {
        offsets[i] = at as u64;
        at = (at + part.len()).next_multiple_of(8);
    }
    let shoff = at;
    let (lo, hi) = match (batch.first(), batch.last()) {
        (Some(first), Some(last)) => (first.at, last.at + u64::from(last.len)),
        _ => (0, 0),
    };
    let mut out: AVec<'a, u8> = Vec::new_in(alloc);
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    u16le(&mut out, c::ET_EXEC);
    u16le(&mut out, match arch {
        Arch::Arm64 => c::EM_AARCH64,
        Arch::X86_64 => c::EM_X86_64,
    });
    u32le(&mut out, 1);
    u64le(&mut out, 0);
    u64le(&mut out, 0);
    u64le(&mut out, shoff as u64);
    u32le(&mut out, 0);
    u16le(&mut out, c::EHDR as u16);
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, c::SHDR as u16);
    u16le(&mut out, 8);
    u16le(&mut out, 7);
    for (i, part) in parts.iter().enumerate() {
        out.resize(offsets[i] as usize, 0);
        out.extend_from_slice(part);
    }
    out.resize(shoff, 0);
    // (type, flags, addr, offset, size, link, info, align, entsize) for sections 0 to 7.
    let headers: [(u32, u64, u64, u64, u64, u32, u32, u64, u64); 8] = [
        (0, 0, 0, 0, 0, 0, 0, 0, 0),
        (c::SHT_NOBITS, c::SHF_ALLOC | c::SHF_EXECINSTR, lo, c::EHDR as u64, hi - lo, 0, 0, 4, 0),
        (c::SHT_SYMTAB, 0, 0, offsets[0], symtab.len() as u64, 3, 1, 8, c::SYM as u64),
        (c::SHT_STRTAB, 0, 0, offsets[1], strtab.len() as u64, 0, 0, 1, 0),
        (c::SHT_PROGBITS, 0, 0, offsets[2], c::ABBREV.len() as u64, 0, 0, 1, 0),
        (c::SHT_PROGBITS, 0, 0, offsets[3], info.len() as u64, 0, 0, 1, 0),
        (c::SHT_PROGBITS, 0, 0, offsets[4], line.len() as u64, 0, 0, 1, 0),
        (c::SHT_STRTAB, 0, 0, offsets[5], c::SHSTRTAB.len() as u64, 0, 0, 1, 0),
    ];
    for (i, &(kind, flags, addr, offset, size, link, sh_info, align, entsize)) in headers.iter().enumerate() {
        u32le(&mut out, c::NAME_AT[i]);
        u32le(&mut out, kind);
        u64le(&mut out, flags);
        u64le(&mut out, addr);
        u64le(&mut out, offset);
        u64le(&mut out, size);
        u32le(&mut out, link);
        u32le(&mut out, sh_info);
        u64le(&mut out, align);
        u64le(&mut out, entsize);
    }
    Ok(out)
}

/// GDB's `struct jit_code_entry`.
#[repr(C)]
pub struct CodeEntry {
    next: *mut CodeEntry,
    prev: *mut CodeEntry,
    symfile_addr: *const u8,
    symfile_size: u64,
}

/// GDB's `struct jit_descriptor`.
#[repr(C)]
pub struct Descriptor {
    version: u32,
    action_flag: u32,
    relevant_entry: *mut CodeEntry,
    first_entry: *mut CodeEntry,
}

/// The descriptor's storage: a debugger reads it by its symbol, so it is one C struct.
#[repr(transparent)]
pub struct DescriptorCell(UnsafeCell<Descriptor>);

// SAFETY: written only by the thread that holds the [`Jit`] claim (`!Send`, `!Sync`), and read
// by a debugger only while that thread is stopped in `__jit_debug_register_code`.
unsafe impl Sync for DescriptorCell {}

pub const JIT_NOACTION: u32 = 0;
pub const JIT_REGISTER_FN: u32 = 1;
pub const JIT_UNREGISTER_FN: u32 = 2;

/// The descriptor a debugger looks up by this name (GDB's manual: version 1, empty).
#[expect(non_upper_case_globals, reason = "GDB's JIT interface names the symbol")]
#[unsafe(no_mangle)]
pub static __jit_debug_descriptor: DescriptorCell =
    DescriptorCell(UnsafeCell::new(Descriptor { version: 1, action_flag: JIT_NOACTION, relevant_entry: null_mut(), first_entry: null_mut() }));

/// The function a debugger puts its breakpoint on. Never inlined, and its body is an empty
/// `asm!` without `nomem`, so a call is a real call and the descriptor's writes before it are
/// not moved past it.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __jit_debug_register_code() {
    // SAFETY: an empty instruction sequence: it touches no register, memory or stack; it is
    // here only to be opaque to the optimizer.
    unsafe { core::arch::asm!("", options(nostack, preserves_flags)) }
}

static CLAIMED: AtomicBool = AtomicBool::new(false);

/// The descriptor's one writer: claimed once, on the thread that places code; `!Send`, `!Sync`.
pub struct Jit {
    _one_thread: PhantomData<*const ()>,
}

impl Jit {
    pub fn claim() -> Result<Jit, JitError> {
        match CLAIMED.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => Ok(Jit { _one_thread: PhantomData }),
            Err(_) => Err(JitError::Taken),
        }
    }

    /// Tells the debugger of `object` (an [`object`]'s bytes); it is told again, to forget it,
    /// when the returned value is dropped.
    pub fn register<'a>(&self, object: AVec<'a, u8>, alloc: Alloc<'a>) -> Registered<'_, 'a> {
        let entry = CodeEntry { next: null_mut(), prev: null_mut(), symfile_addr: object.as_ptr(), symfile_size: object.len() as u64 };
        let mut entry = Box::new_in(entry, alloc);
        let this: *mut CodeEntry = &mut *entry;
        let descriptor = __jit_debug_descriptor.0.get();
        // SAFETY: this thread holds the claim, the descriptor's one writer (`DescriptorCell`'s
        // `Sync`); every entry in its list is a live `Registered`'s box (unlinked on its drop),
        // and `this` is a fresh box that stays where it is until that drop.
        unsafe {
            let first = (*descriptor).first_entry;
            (*this).next = first;
            if !first.is_null() {
                (*first).prev = this;
            }
            (*descriptor).first_entry = this;
            (*descriptor).relevant_entry = this;
            (*descriptor).action_flag = JIT_REGISTER_FN;
        }
        __jit_debug_register_code();
        Registered { entry, _object: object, _jit: self }
    }
}

impl Drop for Jit {
    fn drop(&mut self) {
        CLAIMED.store(false, Ordering::Release);
    }
}

/// A batch the debugger knows of; dropping it tells the debugger to forget it.
pub struct Registered<'j, 'a> {
    entry: Box<CodeEntry, Alloc<'a>>,
    /// The bytes the entry names: kept until the entry is unlinked.
    _object: AVec<'a, u8>,
    _jit: &'j Jit,
}

impl Drop for Registered<'_, '_> {
    fn drop(&mut self) {
        let this: *mut CodeEntry = &mut *self.entry;
        let descriptor = __jit_debug_descriptor.0.get();
        // SAFETY: as in `Jit::register`: the claim's thread (a `Registered` borrows the `!Sync`
        // `Jit`), and `this` is in the list (linked by `register`, unlinked only here).
        unsafe {
            let (next, prev) = ((*this).next, (*this).prev);
            match prev.is_null() {
                true => (*descriptor).first_entry = next,
                false => (*prev).next = next,
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
            (*descriptor).relevant_entry = this;
            (*descriptor).action_flag = JIT_UNREGISTER_FN;
        }
        __jit_debug_register_code();
        // SAFETY: as above; the debugger has read the entry (it was stopped in the call).
        unsafe {
            (*descriptor).relevant_entry = null_mut();
            (*descriptor).action_flag = JIT_NOACTION;
        }
    }
}

/// For the tests: the descriptor's list head and action.
#[cfg(test)]
fn descriptor_now() -> (*const u8, u64, u32) {
    let descriptor = __jit_debug_descriptor.0.get();
    // SAFETY: the tests read it on the claiming thread, between calls.
    unsafe {
        let first = (*descriptor).first_entry;
        match first.is_null() {
            true => (core::ptr::null(), 0, (*descriptor).action_flag),
            false => ((*first).symfile_addr, (*first).symfile_size, (*descriptor).action_flag),
        }
    }
}

#[cfg(test)]
#[path = "jit_debug_tests.rs"]
mod tests;
