//! The program's native library in the launcher (port27/tempo-run/NATIVE.md 3.4, 3.6, 3.7;
//! piece W8): the C, C++ and assembly a program's build scripts compiled, laid out as one shared
//! library record (W7 makes it, W4 writes it), loaded by the launcher before the entry runs.
//!
//! The parent sends `Down::Native { library, words }` once, first, before any `Declare` of the
//! entry. Then, in this order:
//!
//! 1. every pair's slot is stubbed ([`crate::run_mode::Memory::stub`]): its entry thunk exists
//!    and leads to its stub, so C may call it before its body is placed;
//! 2. the record is opened (`dlopen(<store>/<64 hex>, RTLD_NOW | RTLD_LOCAL)`): dyld or ld.so
//!    maps it, binds its imports by flat lookup among the libraries it loads, and runs only
//!    those libraries' initializers (the image's own are in `__blaze_inits`, not
//!    `__mod_init_func`);
//! 3. the descriptor `__blaze_native_v1` is read and checked ([`Descriptor`], tag `BNL2`);
//! 4. word `i` gets the entry thunk of `words[i]`'s slot, then each pointer row's cell gets the
//!    entry thunk of its word's slot: a function has one address, its slot's thunk, whether code
//!    takes it or C data holds it (the coordinator's ruling, 2026-11-16: rule 4, a data pointer
//!    and a code pointer to one function compare equal);
//! 5. the first thread, after the entry is numbered and before it is called, runs the init list
//!    in order ([`run_inits`]): initializers before `main`, as an executable has them.
//!
//! From then on a C name a code record calls or reads is bound by [`bind`] through the handle
//! (`dlsym(handle, name)`: the image, then the libraries it loaded), never `RTLD_DEFAULT`: a name
//! resolves against the program's declared libraries, not against what the launcher process
//! happened to load. A C name before the library is loaded is [`RunModeError::NoNativeLibrary`].
//!
//! Two parts, as run mode has them: the sans-IO part ([`NativeLoad`], [`bind`],
//! [`record_path`], the [`Natives`] seam), tested with a fake (tests/native_lib.rs, no dylib),
//! and the driver ([`DlNatives`], [`run_inits`]), which only performs the effects.
//!
//! macOS private APIs (rule 45): none. `dlopen`, `dlsym`, `dlerror` are POSIX; `_NSGetArgc`,
//! `_NSGetArgv`, `_NSGetEnviron` are declared in the SDK's public `<crt_externs.h>` (the test
//! `ns_get_args_exist` still checks they answer, since this file leans on them).

use core::ffi::c_void;
use core::fmt;

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::RecordId;
use blaze_native::descriptor::DescriptorError;
use blaze_run::SlotId;
#[cfg(target_os = "macos")]
use blaze_sys::posix::thread::{self as dyld, RtldFlags};

use crate::run_mode::{CName, Memory, RunModeError};

/// The descriptor's form has one home, blaze-native's descriptor.rs (BNL2: words, pointer rows,
/// the names table; BNL1 refused by its tag, never read beside it).
pub use blaze_native::descriptor::{Descriptor, LEN as DESCRIPTOR_LEN, MAGIC, MAGIC_BNL1, POINTER_ROW_LEN, PointerRow};
/// The descriptor's name as `dlsym` takes it (it adds Mach-O's leading `_` itself).
pub const DESCRIPTOR_NAME: &core::ffi::CStr = c"__blaze_native_v1";
/// The longest path a record's file has, its NUL included.
pub const PATH_MOST: usize = 1024;
/// The longest C name bound, its NUL included.
pub const NAME_MOST: usize = 512;
/// The longest `dlerror` text kept for the driver's message.
const SAID_MOST: usize = 512;

/// Why the native library could not be loaded, naming the record and the rule (NATIVE.md 3.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeError {
    /// `dlopen` refused the record; its `dlerror` text is [`DlNatives::said`], printed by the
    /// driver beside this, not kept here.
    Open { record: RecordId },
    /// The store's folder with the record's name is `len` bytes, past [`PATH_MOST`].
    Path { record: RecordId, len: usize },
    /// The image exports no `__blaze_native_v1`: not a native library record.
    NoDescriptor { record: RecordId },
    /// The descriptor's tag is `got`, neither BNL2 nor BNL1.
    Magic { record: RecordId, got: u32 },
    /// The descriptor is BNL1, made before C data could hold a Rust function's address: an old
    /// record, rebuilt, never read (rule 26).
    Old { record: RecordId, tag: u32 },
    /// The image has `image` words; the parent declared `declared` (word i is pair i).
    Words { record: RecordId, image: u32, declared: u32 },
    /// Pointer row `row` names word `word`, past the image's `words`.
    PointerWord { record: RecordId, row: u32, word: u32, words: u32 },
    /// Pointer row `row`'s reserved field is not zero: written by another form than this
    /// reader's.
    Reserved { record: RecordId, row: u32 },
    /// The parent sent a second `Native` (this one naming `record`): a run has one library.
    Twice { record: RecordId },
    /// The parent sent no `Native` within `bound_ms` of the launcher's start, or ended first:
    /// the entry cannot run before the library's initializers (NATIVE.md 3.7).
    NotSent { bound_ms: u64 },
    /// Init cell `index` of `record` holds address 0: nothing to call, and calling it would
    /// fault at 0.
    NullInit { record: RecordId, index: u32 },
    /// `dlopen` refused `record` because `symbol`, an import of its C, is defined by none of
    /// the libraries it loads (`RTLD_NOW` binds every import at the load; [`open_refused`]).
    Unresolved { record: RecordId, symbol: CName },
    /// `dlopen` refused `record` for another reason: `said` is dlerror's words after the path,
    /// whole (rule 19: dyld's "tried: '<path>' (missing code signature in <uuid> '<path>'), ..."
    /// names the cause only past its first hundred bytes).
    Refused { record: RecordId, said: Said },
}

/// The loader's words, kept whole up to [`SAID_MOST`] bytes (a C name's [`CName::MOST`] cut
/// dyld's refusal to its path): a longer text is cut at a character boundary and printed with
/// `...`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Said {
    bytes: [u8; SAID_MOST],
    len: u16,
    cut: bool,
}

impl Said {
    /// `text`'s first [`SAID_MOST`] bytes, cut back to a character boundary.
    pub fn new(text: &str) -> Said {
        let mut len = text.len().min(SAID_MOST);
        while !text.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0u8; SAID_MOST];
        bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
        Said { bytes, len: len as u16, cut: len < text.len() }
    }
}

impl fmt::Display for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The bytes are a prefix of a `&str` cut at a character boundary (`Said::new`).
        let text = core::str::from_utf8(&self.bytes[..usize::from(self.len)]).map_err(|_| fmt::Error)?;
        write!(f, "`{text}{}`", if self.cut { "..." } else { "" })
    }
}

/// The typed refusal of a `dlopen` of `record` from dyld's (or ld.so's) `said` text: the name
/// dyld could not bind when it says one (macOS: "symbol not found in flat namespace '_x'" or
/// "Symbol not found: _x"; glibc: "undefined symbol: x"), else its words after the path.
pub fn open_refused(record: RecordId, said: &str) -> NativeError {
    for (lead, end) in [("symbol not found in flat namespace '", '\''), ("Symbol not found: ", '\n'), ("undefined symbol: ", ' ')] {
        if let Some((_, rest)) = said.split_once(lead) {
            let name = rest.split(end).next().unwrap_or(rest).trim_end_matches(['\'', ')', ' ', '\n']);
            return NativeError::Unresolved { record, symbol: CName::new(name) };
        }
    }
    let words = said.split_once("): ").map_or(said, |(_, words)| words);
    NativeError::Refused { record, said: Said::new(words) }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = |f: &mut fmt::Formatter<'_>, record: &RecordId| -> fmt::Result {
            for byte in &record.0[..6] {
                write!(f, "{byte:02x}")?;
            }
            Ok(())
        };
        match self {
            NativeError::Open { record } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                f.write_str(" did not load (`dlopen`, NATIVE.md 3.7)")
            }
            NativeError::Path { record, len } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, "'s file path is {len} bytes, longer than the {PATH_MOST} a path holds here")
            }
            NativeError::NoDescriptor { record } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                f.write_str(" exports no `__blaze_native_v1` (NATIVE.md 3.4)")
            }
            NativeError::Magic { record, got } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, "'s descriptor has tag {got:#010x}, not BNL2 (NATIVE.md 3.4)")
            }
            NativeError::Old { record, tag } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, " has the old descriptor {tag:#010x} (BNL1, no pointer list): an old record, rebuilt (rule 26)")
            }
            NativeError::Words { record, image, declared } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, " has {image} Rust words; the parent declared {declared} (NATIVE.md 3.7)")
            }
            NativeError::PointerWord { record, row, word, words } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, "'s pointer row {row} names word {word} of {words} (NATIVE.md 3.4)")
            }
            NativeError::Reserved { record, row } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, "'s pointer row {row} has a non-zero reserved field (blaze-native descriptor.rs, BNL2)")
            }
            NativeError::Twice { record } => {
                f.write_str("the parent sent a second native library, ")?;
                hex(f, record)?;
                f.write_str(": a run has one (NATIVE.md 3.7)")
            }
            NativeError::NotSent { bound_ms } => write!(
                f,
                "the parent sent no native library within {bound_ms} ms: the entry cannot run before its initializers (NATIVE.md 3.7, rule 18)"
            ),
            NativeError::NullInit { record, index } => {
                f.write_str("the native library record ")?;
                hex(f, record)?;
                write!(f, "'s initializer {index} is address 0 (NATIVE.md 3.4)")
            }
            NativeError::Unresolved { record, symbol } => {
                f.write_str("the program did not start: its native library record ")?;
                hex(f, record)?;
                write!(f, " imports {symbol}, which none of the libraries it links defines (`dlopen`, NATIVE.md 3.7)")
            }
            NativeError::Refused { record, said } => {
                f.write_str("the program did not start: its native library record ")?;
                hex(f, record)?;
                write!(f, " did not load: {said} (`dlopen`, NATIVE.md 3.7)")
            }
        }
    }
}

/// The effects the sans-IO part asks for; the driver ([`DlNatives`]) does them with
/// `dlopen`/`dlsym`, a test with a fake.
pub trait Natives {
    /// Opens `record`: the image's base address.
    fn open(&mut self, record: RecordId) -> Result<u64, NativeError>;
    /// The opened image's descriptor.
    fn descriptor(&mut self, record: RecordId) -> Result<Descriptor, NativeError>;
    /// Pointer row `row` of the opened image, at `address` (`base + pointer_at + 16 * row`).
    fn pointer_row(&mut self, address: u64) -> PointerRow;
    /// Stores `value` in the 8-byte cell at `address` (a word or a pointer row's cell).
    fn write_word(&mut self, address: u64, value: u64);
    /// The address of C name `name` in the image or a library it loaded; `None` when none has
    /// it. `name` is shorter than [`NAME_MOST`] and holds no NUL ([`bind`] checks).
    fn lookup(&mut self, name: &str) -> Option<u64>;
}

/// What run mode keeps once the library is loaded: the base and the init list the first thread
/// runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Loaded {
    pub record: RecordId,
    pub base: u64,
    /// The first init cell's address (`base + init_at`).
    pub inits_at: u64,
    pub inits: u32,
}

/// The load's order (the module's steps 1 to 4).
pub struct NativeLoad;

impl NativeLoad {
    /// Stubs each declared slot through `memory`, opens the record, checks the descriptor,
    /// fills word i with `memory.thunk(slot_i)`, then each pointer row's cell with its word's
    /// thunk. `slots[i]` is the local slot of the parent's pair i.
    pub fn start<N: Natives, M: Memory>(
        record: RecordId,
        slots: &[SlotId],
        natives: &mut N,
        memory: &mut M,
    ) -> Result<Loaded, RunModeError> {
        for &slot in slots {
            memory.stub(slot)?;
        }
        let base = natives.open(record).map_err(RunModeError::Native)?;
        let d = natives.descriptor(record).map_err(RunModeError::Native)?;
        let refuse = |why| Err(RunModeError::Native(why));
        match d.check() {
            Ok(()) => {}
            Err(DescriptorError::Old { tag }) => return refuse(NativeError::Old { record, tag }),
            Err(_) => return refuse(NativeError::Magic { record, got: d.magic }),
        }
        let declared = u32::try_from(slots.len()).unwrap_or(u32::MAX);
        if d.words != declared {
            return refuse(NativeError::Words { record, image: d.words, declared });
        }
        for (index, &slot) in slots.iter().enumerate() {
            natives.write_word(base + d.word_at + 8 * index as u64, memory.thunk(slot));
        }
        // Checked whole before the first cell is written: a refused row leaves no half-filled
        // data behind (the process stops on the error anyway, but nothing ran with it).
        for row in 0..d.pointers {
            let r = natives.pointer_row(base + d.pointer_at + POINTER_ROW_LEN * u64::from(row));
            if r.pad != 0 {
                return refuse(NativeError::Reserved { record, row });
            }
            if r.word >= d.words {
                return refuse(NativeError::PointerWord { record, row, word: r.word, words: d.words });
            }
        }
        for row in 0..d.pointers {
            let r = natives.pointer_row(base + d.pointer_at + POINTER_ROW_LEN * u64::from(row));
            natives.write_word(base + r.at, memory.thunk(slots[r.word as usize]));
        }
        Ok(Loaded { record, base, inits_at: base + d.init_at, inits: d.inits })
    }
}

/// Binds C name `name` (code record symbol `symbol`, named by record `record`) through the
/// loaded library: its address. `natives` is `None` before the parent's `Native` was loaded.
pub fn bind<N: Natives>(natives: Option<&mut N>, record: RecordId, symbol: CodeSymbol, name: &str) -> Result<u64, RunModeError> {
    let Some(natives) = natives else {
        return Err(RunModeError::NoNativeLibrary { symbol });
    };
    if name.len() >= NAME_MOST || name.as_bytes().contains(&0) {
        return Err(RunModeError::Outside { record, symbol, name: CName::new(name), why: "its name is longer than 511 bytes or holds a NUL" });
    }
    natives.lookup(name).ok_or(RunModeError::Outside {
        record,
        symbol,
        name: CName::new(name),
        why: "neither the program's native library nor a library it loads defines it (`dlsym` on its handle)",
    })
}

/// The store's file of `id` under `folder` (no trailing `/`), NUL-terminated, in `out`: its
/// length without the NUL. The one place a record's file name is made (run_mode's `Folder`
/// reads through it too).
pub fn record_path(folder: &[u8], id: RecordId, out: &mut [u8; PATH_MOST]) -> Result<usize, RunModeError> {
    let len = folder.len() + 1 + 64;
    if len + 1 > PATH_MOST {
        return Err(RunModeError::StorePath { len: len + 1 });
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[..folder.len()].copy_from_slice(folder);
    out[folder.len()] = b'/';
    for (index, byte) in id.0.iter().enumerate() {
        let at = folder.len() + 1 + index * 2;
        out[at] = HEX[usize::from(byte >> 4)];
        out[at + 1] = HEX[usize::from(byte & 0xf)];
    }
    out[len] = 0;
    Ok(len)
}

/// A record source that keeps records as files of one folder: the driver `dlopen`s the native
/// library's file in place.
pub trait RecordFiles {
    /// The folder, without a trailing `/`.
    fn folder(&self) -> &[u8];
}

/// The driver's [`Natives`]: `dlopen` of the store's file, `dlsym` on its handle.
/// It owns a copy of the folder's path (no borrow), so run mode's memory can keep it for the
/// process's life.
pub struct DlNatives {
    folder: [u8; PATH_MOST],
    folder_len: usize,
    /// The open image, once [`Natives::open`] succeeded. Never closed: the program's code runs
    /// from it until the process ends.
    handle: *mut c_void,
    said: [u8; SAID_MOST],
    said_len: usize,
}

impl DlNatives {
    /// `folder` is the store's folder, without a trailing `/`; refused when a record's path
    /// under it would not fit [`PATH_MOST`].
    pub fn new(folder: &[u8]) -> Result<DlNatives, RunModeError> {
        let len = folder.len() + 1 + 64 + 1;
        if len > PATH_MOST {
            return Err(RunModeError::StorePath { len });
        }
        let mut copy = [0u8; PATH_MOST];
        copy[..folder.len()].copy_from_slice(folder);
        Ok(DlNatives { folder: copy, folder_len: folder.len(), handle: core::ptr::null_mut(), said: [0; SAID_MOST], said_len: 0 })
    }

    /// The `dlerror` text of the last failed `dlopen`, for the driver's message.
    pub fn said(&self) -> &str {
        core::str::from_utf8(&self.said[..self.said_len]).unwrap_or("(dlerror's text is not UTF-8)")
    }

    fn keep_dlerror(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: the loader's thread-local error text is copied before the next loader call.
        let text = unsafe { dyld::dlerror() };
        #[cfg(target_os = "linux")]
        // SAFETY: `dlerror` returns null or a NUL-terminated string owned by the dynamic loader,
        // valid until the next dl* call on this thread; it is copied out before any.
        let text = unsafe { libc::dlerror() };
        self.said_len = 0;
        if text.is_null() {
            return;
        }
        // SAFETY: as above: a NUL-terminated string.
        let bytes = unsafe { core::ffi::CStr::from_ptr(text) }.to_bytes();
        let n = bytes.len().min(SAID_MOST);
        self.said[..n].copy_from_slice(&bytes[..n]);
        self.said_len = n;
    }
}

impl Natives for DlNatives {
    fn open(&mut self, record: RecordId) -> Result<u64, NativeError> {
        if !self.handle.is_null() {
            return Err(NativeError::Twice { record });
        }
        let mut path = [0u8; PATH_MOST];
        if record_path(&self.folder[..self.folder_len], record, &mut path).is_err() {
            // `new` checked the length; kept named all the same.
            return Err(NativeError::Path { record, len: self.folder_len + 66 });
        }
        // `path` is NUL-terminated (record_path). Assumptions this load rests on:
        // (1) the file is a record, written once and never rewritten in place (the store's
        //     rule 51): dyld maps its pages and checks their code-signing hashes as they fault
        //     in, so a file changed under the mapping would kill the process;
        // (2) it is ad hoc signed (W4, `sign::sign`) and the launcher runs without the
        //     hardened runtime's library validation (run_mode's "ad hoc signed runs"), so dyld
        //     accepts a library signed by no team;
        // (3) its initializers are not `__mod_init_func` (W4 writes none), so this call runs
        //     only the libraries' own initializers, before any Rust word is filled;
        // (4) its imports bind by flat lookup among the libraries it loads (`RTLD_NOW`: every
        //     one now, so a missing name fails here, named by dlerror, not at a later call);
        // (5) `RTLD_LOCAL`: its names do not join the process's global lookup, so nothing
        //     loaded later binds against the program's C code by accident.
        #[cfg(target_os = "macos")]
        // SAFETY: the path names the NUL-terminated library record described above.
        let handle = unsafe { dyld::dlopen(path.as_ptr().cast(), (RtldFlags::NOW | RtldFlags::LOCAL).bits()) };
        #[cfg(target_os = "linux")]
        // SAFETY: the path names the NUL-terminated library record described above.
        let handle = unsafe { libc::dlopen(path.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            self.keep_dlerror();
            return Err(NativeError::Open { record });
        }
        self.handle = handle;
        // The base: the descriptor's image-relative addresses are from the image's first byte,
        // where its Mach-O header (or ELF header) is. `dladdr` of the descriptor names that
        // image's load address.
        #[cfg(target_os = "macos")]
        // SAFETY: a lookup on a live handle by a NUL-terminated name.
        let descriptor = unsafe { dyld::dlsym(handle, DESCRIPTOR_NAME.as_ptr()) };
        #[cfg(target_os = "linux")]
        // SAFETY: a lookup on a live handle by a NUL-terminated name.
        let descriptor = unsafe { libc::dlsym(handle, DESCRIPTOR_NAME.as_ptr()) };
        if descriptor.is_null() {
            return Err(NativeError::NoDescriptor { record });
        }
        #[cfg(target_os = "macos")]
        let info = dyld::address_info(descriptor).ok_or(NativeError::NoDescriptor { record })?;
        #[cfg(target_os = "linux")]
        let info = {
            // SAFETY: an all-zero `Dl_info` is a valid value for the call to fill.
            let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
            // SAFETY: `descriptor` is an address inside the image just loaded; `info` a live local.
            let found = unsafe { libc::dladdr(descriptor, &mut info) };
            if found == 0 {
                return Err(NativeError::NoDescriptor { record });
            }
            info
        };
        if info.dli_fbase.is_null() {
            return Err(NativeError::NoDescriptor { record });
        }
        Ok(info.dli_fbase as u64)
    }

    fn descriptor(&mut self, record: RecordId) -> Result<Descriptor, NativeError> {
        if self.handle.is_null() {
            return Err(NativeError::Open { record });
        }
        #[cfg(target_os = "macos")]
        // SAFETY: a lookup on the live handle by a NUL-terminated name.
        let at = unsafe { dyld::dlsym(self.handle, DESCRIPTOR_NAME.as_ptr()) };
        #[cfg(target_os = "linux")]
        // SAFETY: a lookup on the live handle by a NUL-terminated name.
        let at = unsafe { libc::dlsym(self.handle, DESCRIPTOR_NAME.as_ptr()) };
        if at.is_null() {
            return Err(NativeError::NoDescriptor { record });
        }
        // SAFETY: the exported `__blaze_native_v1` is the first DESCRIPTOR_LEN bytes of the
        // image's `__data` (W3 layout: the descriptor starts Data, 8-aligned), mapped readable
        // for the image's life. Read unaligned all the same: the reader does not trust the
        // writer's alignment.
        Ok(unsafe { (at as *const Descriptor).read_unaligned() })
    }

    fn pointer_row(&mut self, address: u64) -> PointerRow {
        // SAFETY: `address` is `base + pointer_at + 16 * row` with `row < pointers`, the
        // descriptor's own list inside the image's mapped data (W3 layout); read unaligned.
        unsafe { (address as *const PointerRow).read_unaligned() }
    }

    fn write_word(&mut self, address: u64, value: u64) {
        // SAFETY: `address` is a word cell (`base + word_at + 8 * i`, `i < words`) or a pointer
        // row's cell (`base + at`), both in the image's writable `__DATA` (W4: `__blaze_words`
        // and `__data` are rw-, never `SG_READ_ONLY`), 8-aligned by the layout. No thread runs
        // the image's code yet: the init list and the entry run after every cell is written.
        unsafe { (address as *mut u64).write_volatile(value) }
    }

    fn lookup(&mut self, name: &str) -> Option<u64> {
        if self.handle.is_null() || name.len() >= NAME_MOST || name.as_bytes().contains(&0) {
            // `bind` refused a long or NUL-holding name first, and binds only once loaded.
            return None;
        }
        let mut cname = [0u8; NAME_MOST];
        cname[..name.len()].copy_from_slice(name.as_bytes());
        #[cfg(target_os = "macos")]
        // SAFETY: the handle is live and the zero following the copied name terminates it.
        let address = unsafe { dyld::dlsym(self.handle, cname.as_ptr().cast()) };
        #[cfg(target_os = "linux")]
        // SAFETY: a lookup on the live handle by a NUL-terminated name: the image, then the
        // libraries it loaded, in load order (dlsym(3) with a handle).
        let address = unsafe { libc::dlsym(self.handle, cname.as_ptr().cast()) };
        (!address.is_null()).then_some(address as u64)
    }
}

/// Runs the library's initializers in order on the calling thread: the process's first thread,
/// after the entry is numbered and before it is called (NATIVE.md 3.4, 3.7).
#[cfg(target_os = "macos")]
pub fn run_inits(loaded: Loaded) -> Result<(), RunModeError> {
    use blaze_sys::posix::proc::{_NSGetArgc, _NSGetArgv, _NSGetEnviron};
    use core::ffi::{c_char, c_int};
    type Init = extern "C" fn(c_int, *const *const c_char, *const *const c_char);
    // SAFETY: `_NSGetArgc`, `_NSGetArgv`, `_NSGetEnviron` (<crt_externs.h>) return the
    // process's own argc, argv and environ, set up by dyld before any code ran; the program
    // runs in this process, so they are its arguments (the entry's `argv` is the same list).
    let (argc, argv, envp) = unsafe { (*_NSGetArgc(), *_NSGetArgv(), *_NSGetEnviron()) };
    for index in 0..loaded.inits {
        let cell = loaded.inits_at + 8 * u64::from(index);
        // SAFETY: init cell `index < inits` of the image's `__blaze_inits`, 8-aligned and
        // readable for the image's life; dyld rebased it at `dlopen` (W3 makes every init cell
        // a rebase fixup), so it holds an absolute address in the image's text.
        let address = unsafe { (cell as *const u64).read_volatile() };
        if address == 0 {
            return Err(RunModeError::Native(NativeError::NullInit { record: loaded.record, index }));
        }
        // SAFETY: a C initializer of a member the program's link pulled (`__mod_init_func`'s
        // entry, moved to `__blaze_inits`). dyld calls these as `void (int argc, char **argv,
        // char **envp, char **apple, const struct ProgramVars *)`; the first three are passed
        // here, `apple` and `vars` are not (only libSystem's own initializers read them, and
        // libSystem is never a member); an initializer declared `void (void)` ignores the
        // arguments (AAPCS64: arguments are registers the callee may ignore).
        let init: Init = unsafe { core::mem::transmute(address as usize) };
        init(argc, argv.cast_const().cast(), envp.cast_const().cast());
    }
    Ok(())
}

/// Linux: the native library is ELF (W5, not built), so an image with initializers is refused
/// by name rather than called with arguments this driver has not checked.
#[cfg(not(target_os = "macos"))]
pub fn run_inits(loaded: Loaded) -> Result<(), RunModeError> {
    match loaded.inits {
        0 => Ok(()),
        _ => Err(RunModeError::NotBuilt { what: "a native library's initializers on Linux (NATIVE.md W5)" }),
    }
}
