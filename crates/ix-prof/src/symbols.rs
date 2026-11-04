//! Names for code addresses, found when a profile is written, never while sampling: the
//! executable's symbol table and DWARF (functions, inline chains, files and lines, through
//! `addr2line`), its load slide from the dynamic loader, and `dladdr` for code in shared
//! libraries. [`names`] is the short form blaze-profile's dumps use.
//!
//! The executable's symbol table is read once a process ([`Table`]): the file is mapped, not
//! read, and each symbol is its start and the offset of its name in the mapping. Until
//! 2026-11-03 every [`names`] call (each 5 s flush of `blaze serve`'s history) parsed it again
//! through small `read` and `lseek` calls and copied every name: 23% of an idle daemon's
//! history thread.

use std::{
    collections::BTreeMap,
    ops::Range,
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_inproc_rt::live::{self, HOST_ARCH};
use parking_lot::Mutex;

use crate::{
    core::Symbolize,
    profile::{Frame, FrameKind},
};

type Reader = gimli::EndianArcSlice<gimli::RunTimeEndian>;

/// The demangled name, without the hash (`{:#}`).
pub fn demangle(name: &str) -> String {
    match rustc_demangle::try_demangle(name) {
        Ok(it) => format!("{it:#}"),
        Err(_) => name.strip_prefix('_').filter(|_| cfg!(target_os = "macos")).unwrap_or(name).to_owned(),
    }
}

fn load_dwarf<'d, R: object::ReadRef<'d>>(
    object: &object::File<'d, R>,
    endian: gimli::RunTimeEndian,
) -> Option<addr2line::Context<Reader>> {
    use object::{Object, ObjectSection};
    let dwarf = gimli::Dwarf::load(|id: gimli::SectionId| -> Result<Reader, gimli::Error> {
        let data: Arc<[u8]> = object
            .section_by_name(id.name())
            .and_then(|s| s.uncompressed_data().ok())
            .map(|it| Arc::from(&*it))
            .unwrap_or_else(|| Arc::from(&[][..]));
        Ok(gimli::EndianArcSlice::new(data, endian))
    })
    .ok()?;
    addr2line::Context::from_dwarf(dwarf).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Times this process parsed its executable's symbol table: at most once.
static PARSES: AtomicU64 = AtomicU64::new(0);

/// How many times this process has parsed its executable's symbol table: 1 once anything was
/// named, never more.
pub fn parses() -> u64 {
    PARSES.load(Ordering::Relaxed)
}

/// The executable's text symbols, read once a process ([`own`]).
///
/// Memory: the file is mapped read-only for the process's life, so its bytes are the page
/// cache's (clean, evictable, resident only where touched: the symbol and string tables while
/// they are parsed, then the names looked up), never the heap. The heap holds 20 bytes a
/// symbol (`starts` and `rest`; blaze's own 227 MB binary has 267,519 text symbols: 5.4 MB),
/// 24 bytes a symbol more while the table is sorted, once, and a demangled name for each
/// function ever named (`demangled`, bounded by the functions samples land in).
struct Table {
    path: String,
    /// The executable's bytes, mapped for the process's life.
    data: &'static [u8],
    build_id: Option<String>,
    /// The file addresses of the executable's code sections, by start: a symbol names only
    /// addresses of its own section.
    code: Vec<Range<u64>>,
    /// A live image's slot stubs' section (blaze-link's, [`live::STUBS_SECTION_MACHO`]): its
    /// file addresses and where they are in `data`.
    stubs: Option<(Range<u64>, u64)>,
    /// Each symbol's start (a file address), ascending, one symbol a start.
    starts: Vec<u64>,
    /// Each symbol's size (0: not known, as Mach-O gives none), and its mangled name's offset
    /// and length in `data`, by index into `starts`.
    rest: Vec<(u32, u32, u32)>,
    /// Demangled names by index into `starts`, made the first time each is asked for.
    demangled: Mutex<BTreeMap<u32, Box<str>>>,
}

impl Table {
    /// Maps the executable at `path` and reads its text symbols. An error names the file and
    /// what failed.
    fn read(path: &Path) -> Result<Table, String> {
        use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};
        PARSES.fetch_add(1, Ordering::Relaxed);
        let data = map(path)?;
        let object = object::File::parse(data).map_err(|error| format!("parsing {}: {error}", path.display()))?;
        // Where a name lies in the mapping: object hands out slices of it.
        let place = |name: &[u8]| -> Option<(u32, u32)> {
            let at = (name.as_ptr() as usize).checked_sub(data.as_ptr() as usize)?;
            let end = at.checked_add(name.len())?;
            std::str::from_utf8(data.get(at..end)?).ok()?;
            Some((u32::try_from(at).ok()?, u32::try_from(name.len()).ok()?))
        };
        // alloc: the symbols while they are sorted, once a process
        let mut symbols: Vec<(u64, u32, u32, u32)> = object
            .symbols()
            .filter(|it| it.kind() == SymbolKind::Text && it.address() != 0)
            .filter_map(|it| {
                let (at, len) = place(it.name_bytes().ok()?)?;
                Some((it.address(), u32::try_from(it.size()).unwrap_or(u32::MAX), at, len))
            })
            .collect();
        let name = |it: &(u64, u32, u32, u32)| &data[it.2 as usize..][..it.3 as usize];
        symbols.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| name(a).cmp(name(b))));
        symbols.dedup_by_key(|it| it.0);
        // alloc: the table's starts, once a process
        let starts = symbols.iter().map(|it| it.0).collect();
        // alloc: the table's sizes and names, once a process
        let rest = symbols.iter().map(|it| (it.1, it.2, it.3)).collect();
        let build_id = match object.build_id() {
            Ok(Some(id)) => Some(hex(id)),
            _ => object.mach_uuid().ok().flatten().map(|it| hex(&it)),
        };
        // alloc: the code sections' ranges, once a process
        let mut code: Vec<Range<u64>> = object
            .sections()
            .filter(|it| it.kind() == SectionKind::Text && it.size() > 0)
            .map(|it| it.address()..it.address().saturating_add(it.size()))
            .collect();
        code.sort_unstable_by_key(|it| it.start);
        let stubs = object
            .sections()
            .find(|it| matches!(it.name(), Ok(live::STUBS_SECTION_MACHO | live::STUBS_SECTION_ELF)))
            .and_then(|it| Some((it.address()..it.address().saturating_add(it.size()), it.file_range()?.0)));
        Ok(Table {
            path: path.to_string_lossy().into_owned(),
            data,
            build_id,
            code,
            stubs,
            starts,
            rest,
            demangled: Mutex::new(BTreeMap::new()),
        })
    }

    /// The executable's DWARF (as large as the rest of the binary, read and parsed only for a
    /// caller that wants lines and inline chains), or its `.dSYM` bundle's on macOS.
    fn dwarf(&self) -> Option<addr2line::Context<Reader>> {
        use object::{Object, ObjectSection};
        let object = object::File::parse(self.data).ok()?;
        let endian =
            if object.is_little_endian() { gimli::RunTimeEndian::Little } else { gimli::RunTimeEndian::Big };
        if object.section_by_name(".debug_info").is_some_and(|s| s.size() > 0)
            && let Some(dwarf) = load_dwarf(&object, endian)
        {
            return Some(dwarf);
        }
        // macOS keeps DWARF beside the binary in a .dSYM bundle when dsymutil ran.
        if !cfg!(target_vendor = "apple") {
            return None;
        }
        let path = Path::new(&self.path);
        let name = path.file_name()?.to_owned();
        let mut dsym = path.as_os_str().to_owned();
        dsym.push(".dSYM");
        let dsym = std::path::PathBuf::from(dsym).join("Contents/Resources/DWARF").join(name);
        let file = std::fs::File::open(&dsym).ok()?;
        let cache = object::ReadCache::new(file);
        let object = object::File::parse(&cache).ok()?;
        load_dwarf(&object, endian)
    }

    /// The index of the symbol `at` (a file address) falls in: the last to start at or before
    /// it in the code section that holds it. Past the next function's start the address is
    /// not this one's, nor past the end of the function's section (Mach-O gives no sizes):
    /// before 2026-11-03 an address of a live image's stubs, the section after `__text`, was
    /// named for `__text`'s last function, 116 MB on.
    fn symbol(&self, at: u64) -> Option<usize> {
        let section = self.code.partition_point(|it| it.start <= at).checked_sub(1)?;
        let section = self.code[section].clone();
        if !section.contains(&at) {
            return None;
        }
        let i = self.starts.partition_point(|&it| it <= at).checked_sub(1)?;
        let (start, size) = (self.starts[i], self.rest[i].0);
        (start >= section.start && (size == 0 || at < start.saturating_add(u64::from(size)))).then_some(i)
    }

    /// What `at` (a file address) is when it is in a live image's slot stubs, with the image
    /// loaded `slide` from its file: the slot, how far into its stub, and the symbol of the
    /// code the slot holds now. A function pointer of a live image is its slot's stub
    /// ([`live::stub_slot`]), never the function's code, which only the jump table names: the
    /// loader's table of the image's jump table ([`live::log::Header::target`]). None past
    /// the slot stubs (the log stubs, lazy compilation's), and for bytes that are no stub.
    fn stub(&self, at: u64, slide: u64) -> Option<Stub> {
        let (range, file) = self.stubs.clone()?;
        if !range.contains(&at) {
            return None;
        }
        let size = live::slot_stub_bytes(HOST_ARCH)?;
        let slot = (at - range.start) / size;
        let start = range.start + slot * size;
        let from = usize::try_from(file + (start - range.start)).ok()?;
        let bytes = self.data.get(from..from.checked_add(usize::try_from(size).ok()?)?)?;
        // The section's start is slot 0's stub: the stub there says it is this slot's.
        let slot = u32::try_from(slot).ok().filter(|&it| live::stub_slot(HOST_ARCH, bytes) == Some(it))?;
        let target = live::log::Header::this().map(|it| it.target(slot));
        let code = match target {
            Ok(Some(target)) => match target.checked_sub(slide).and_then(|it| self.symbol(it)) {
                Some(i) => Code::Symbol(i),
                None => Code::Unnamed(target),
            },
            Ok(None) => Code::Unread(format!("slot {slot} is past the jump table")),  // alloc: a stub's name, once an address
            Err(why) => Code::Unread(why),
        };
        Some(Stub { slot, offset: at - start, code })
    }

    /// The name of `at` (a file address): its function's with the offset into it, or the
    /// stub's ([`Table::stub`]); none for an address no symbol or stub covers.
    fn name(&self, at: u64, slide: u64) -> Option<String> {
        if let Some(stub) = self.stub(at, slide) {
            return Some(format!("{}+{:#x}", stub.function(self), stub.offset));
        }
        let i = self.symbol(at)?;
        Some(self.demangled(i, |name| format!("{name}+{:#x}", at - self.starts[i])))
    }

    /// Symbol `i`'s mangled name, checked UTF-8 as the table was read.
    fn mangled(&self, i: usize) -> &'static str {
        let (_, at, len) = self.rest[i];
        let data: &'static [u8] = self.data;
        std::str::from_utf8(&data[at as usize..][..len as usize]).unwrap_or_default()
    }

    /// `f` of symbol `i`'s demangled name, demangled once a process.
    fn demangled<R>(&self, i: usize, f: impl FnOnce(&str) -> R) -> R {
        let mut demangled = self.demangled.lock();
        let key = u32::try_from(i).unwrap_or(u32::MAX);
        // alloc: a function's demangled name, once a process a function named
        let name = demangled.entry(key).or_insert_with(|| demangle(self.mangled(i)).into_boxed_str());
        f(name)
    }
}

/// An address in a live image's slot stubs ([`Table::stub`]).
struct Stub {
    slot: u32,
    /// How far into the slot's stub the address is.
    offset: u64,
    code: Code,
}

/// The code a slot holds.
enum Code {
    /// A function of the symbol table, by index.
    Symbol(usize),
    /// Code at this address (slid) no symbol names: placed while the program runs.
    Unnamed(u64),
    /// The jump table could not be read, saying why.
    Unread(String),
}

impl Stub {
    /// What the stub is called: the stub of its slot's function.
    fn function(&self, table: &Table) -> String {
        let slot = self.slot;
        match &self.code {
            Code::Symbol(i) => table.demangled(*i, |name| format!("stub of {name}")),
            Code::Unnamed(at) => format!("stub of slot {slot}, whose code at {at:#x} no symbol names"),
            Code::Unread(why) => format!("stub of slot {slot} (its jump table unread: {why})"),
        }
    }
}

/// The executable, mapped read-only for the process's life.
#[cfg(unix)]
fn map(path: &Path) -> Result<&'static [u8], String> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).map_err(|error| format!("opening {}: {error}", path.display()))?;
    let len = file.metadata().map_err(|error| format!("reading the size of {}: {error}", path.display()))?.len();
    let len = usize::try_from(len).map_err(|_| format!("{} is {len} bytes, past the address space", path.display()))?;
    if len == 0 {
        return Err(format!("{} is empty", path.display()));
    }
    // SAFETY: a new read-only private mapping of `len` bytes of a file opened above; the
    // mapping outlives the descriptor, which closes as `file` drops.
    let at = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0)
    };
    if at == libc::MAP_FAILED {
        return Err(format!("mapping {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: the mapping is `len` readable bytes and is never unmapped, so it lives as long as
    // the process. The file is the running executable, which is not written in place while it
    // runs: Linux refuses (`ETXTBSY`), and on macOS the loader maps the process's own code from
    // the same file, so a write in place would break the process with or without this mapping.
    // A rebuild that replaces the file (a new inode) leaves this mapping on the old one, whose
    // symbols are the ones the running code has.
    Ok(unsafe { std::slice::from_raw_parts(at.cast::<u8>(), len) })
}

#[cfg(not(unix))]
fn map(path: &Path) -> Result<&'static [u8], String> {
    // alloc: the executable's bytes, once a process
    let bytes = std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    Ok(bytes.leak())
}

/// The executable's symbol table, read the first time anything asks: `None` when it can't be,
/// said once as a warning naming why, and its addresses are then named by their offset.
fn own() -> Option<&'static Table> {
    static OWN: OnceLock<Result<Table, String>> = OnceLock::new();
    OWN.get_or_init(|| {
        let table = std::env::current_exe()
            .map_err(|error| format!("finding this process's executable: {error}"))
            .and_then(|path| Table::read(&path));
        if let Err(why) = &table {
            // Said on stderr as ix-prof's other failures are: a symbolizer runs inside the
            // recorder's lock, and a tracing event there would reach ix-prof's own layer.
            eprintln!("ix-prof: no symbol table, so its addresses are named by offset: {why}");
        }
        table
    })
    .as_ref()
    .ok()
}

/// Where this executable is loaded, once a process.
fn own_base() -> Option<u64> {
    static BASE: OnceLock<Option<u64>> = OnceLock::new();
    *BASE.get_or_init(|| image_base(image_base as fn(u64) -> Option<u64> as usize as u64))
}

/// The executable: its symbol table ([`own`]) and, for a [`Symbolizer`], its DWARF.
struct Image {
    table: &'static Table,
    dwarf: Option<addr2line::Context<Reader>>,
}

impl Image {
    fn read() -> Option<Image> {
        let table = own()?;
        Some(Image { table, dwarf: table.dwarf() })
    }

    /// `at`'s frames, outermost (the real function) first, the image `slide` from its file.
    fn frames(&self, at: u64, slide: u64) -> Vec<Frame> {
        let table = self.table;
        let symbol = table.symbol(at);
        let base = |function: String| {
            let mut f = Frame::named(function, FrameKind::Native);
            f.module = Some(table.path.clone());
            f.build_id = table.build_id.clone();
            f.address = Some(at);
            f
        };
        // A stub is no function's code: one frame naming it, never DWARF's lines of another.
        if let Some(stub) = table.stub(at, slide) {
            let mut f = base(stub.function(table));
            if let Code::Symbol(i) = stub.code {
                f.system_name = Some(table.mangled(i).to_owned());
            }
            return vec![f];
        }
        let mut out = Vec::new();
        if let Some(ctx) = &self.dwarf
            && let Ok(mut frames) = ctx.find_frames(at).skip_all_loads()
        {
            while let Ok(Some(frame)) = frames.next() {
                let name = frame
                    .function
                    .as_ref()
                    .and_then(|it| it.raw_name().ok())
                    .map(|it: std::borrow::Cow<'_, str>| demangle(&it))
                    .or_else(|| symbol.map(|i| table.demangled(i, str::to_owned)))
                    .unwrap_or_else(|| format!("{at:#x}"));
                let mut f = base(name);
                if let Some(loc) = &frame.location {
                    f.file = loc.file.map(str::to_owned);
                    f.line = loc.line;
                    f.column = loc.column;
                }
                out.push(f);
            }
        }
        if out.is_empty() {
            out.push(base(symbol.map_or_else(|| format!("{at:#x}"), |i| table.demangled(i, str::to_owned))));
        }
        // addr2line gives the innermost first.
        out.reverse();
        let depth = out.len();
        for (i, f) in out.iter_mut().enumerate() {
            f.inline_depth = i as u16;
            if i == 0 {
                f.system_name = symbol.map(|it| table.mangled(it).to_owned()).filter(|it| *it != f.function);
            }
        }
        debug_assert_eq!(depth, out.len());
        out
    }
}

/// Symbolizes addresses of this process, with lines and inline chains from its DWARF.
pub struct Symbolizer {
    image: Option<Option<Image>>,
    slide: Option<u64>,
    own_base: Option<u64>,
    cache: BTreeMap<u64, Vec<Frame>>,
}

impl Default for Symbolizer {
    fn default() -> Symbolizer {
        Symbolizer::new()
    }
}

impl Symbolizer {
    pub fn new() -> Symbolizer {
        Symbolizer { image: None, slide: slide(), own_base: own_base(), cache: BTreeMap::new() }
    }

    fn image(&mut self) -> Option<&Image> {
        self.image.get_or_insert_with(Image::read).as_ref()
    }

    fn lookup(&mut self, pc: u64) -> Vec<Frame> {
        let own = self.own_base;
        let in_own = loaded(pc).is_none_or(|info| Some(info.0) == own);
        if in_own
            && let Some(slide) = self.slide
            && let Some(at) = pc.checked_sub(slide)
            && let Some(image) = self.image()
        {
            let frames = image.frames(at, slide);
            if frames.first().is_some_and(|f| !f.function.starts_with("0x")) {
                return frames;
            }
        }
        vec![shared_frame(pc, own)]
    }
}

impl Symbolize for Symbolizer {
    fn symbolize(&mut self, addresses: &[u64]) -> Vec<Vec<Frame>> {
        addresses
            .iter()
            .map(|pc| {
                if let Some(it) = self.cache.get(pc) {
                    return it.clone();
                }
                let frames = self.lookup(*pc);
                self.cache.insert(*pc, frames.clone());
                frames
            })
            .collect()
    }

    fn symbolized(&self) -> bool {
        true
    }
}

/// A name for each of `pcs`: the function demangled with its offset, or the library and
/// offset, or the bare address. What blaze-profile's dumps print.
///
/// The symbol table only, read once a process ([`own`]): blaze-profile names a flush's samples
/// with this every flush, and the DWARF it never reads was 1.4 GiB of heap each time building
/// blaze's own tree.
pub fn names(pcs: &[u64]) -> BTreeMap<u64, String> {
    let (slide, base) = (slide(), own_base());
    let mut out = BTreeMap::new();
    for &pc in pcs {
        if out.contains_key(&pc) {
            continue;
        }
        let in_own = loaded(pc).is_none_or(|info| Some(info.0) == base);
        let name = match (in_own, slide) {
            (true, Some(slide)) => pc.checked_sub(slide).and_then(|at| own()?.name(at, slide)),
            _ => None,
        };
        out.insert(pc, name.unwrap_or_else(|| shared(pc, base)));
    }
    out
}

/// A name for each of `pcs`, return addresses of this same executable as another process
/// loaded it, `slide` from its file's addresses (a process that ran this executable and is
/// gone: blaze-profile's crash records). An address in the executable's code is named as
/// [`names`] names it; any other (a shared library's in that process, whose loads are not
/// known here) is its bare address.
pub fn names_slid(pcs: &[u64], slide: u64) -> BTreeMap<u64, String> {
    let ours = self::slide();
    let mut out = BTreeMap::new();
    for &pc in pcs {
        let name = pc.checked_sub(slide).zip(ours).and_then(|(at, ours)| own()?.name(at, ours));
        out.insert(pc, name.unwrap_or_else(|| format!("{pc:#x}")));
    }
    out
}

/// How far this executable was loaded from the addresses its file gives, once a process: what a
/// record of raw addresses keeps so they can be named later ([`names_slid`]).
pub fn own_slide() -> Option<u64> {
    slide()
}

/// Where this executable is loaded (its first byte's address), once a process.
pub fn own_load_base() -> Option<u64> {
    own_base()
}

/// How far the executable was loaded from the addresses its file gives.
#[cfg(target_os = "macos")]
fn slide() -> Option<u64> {
    // SAFETY: image 0 is the main executable, which dyld loaded before `main`.
    let slide = unsafe { libc::_dyld_get_image_vmaddr_slide(0) };
    Some(slide as u64)
}

#[cfg(target_os = "linux")]
fn slide() -> Option<u64> {
    unsafe extern "C" fn first(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        out: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: the loader gives a valid entry for the length of the call, and `out` is the
        // `u64` `slide` passed below.
        let addr = unsafe { (*info).dlpi_addr };
        // SAFETY: as above, `out` points at a live `u64`.
        unsafe { *out.cast::<u64>() = addr };
        1
    }
    let mut slide = 0u64;
    // SAFETY: `first` writes only through the pointer it is given, which is `slide`'s, alive
    // for the call; the first object the loader lists is the main executable.
    unsafe { libc::dl_iterate_phdr(Some(first), std::ptr::from_mut(&mut slide).cast()) };
    Some(slide)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn slide() -> Option<u64> {
    None
}

/// What the loader says of `pc`: the base of its object, the object's path, the nearest
/// exported symbol and its address.
#[cfg(unix)]
fn loaded(pc: u64) -> Option<(u64, Option<String>, Option<String>, u64)> {
    let mut info = libc::Dl_info {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a `Dl_info` to fill; `dladdr` only reads the loader's tables.
    if unsafe { libc::dladdr(pc as *const libc::c_void, &mut info) } == 0 {
        return None;
    }
    let text = |ptr: *const libc::c_char| {
        // SAFETY: `dladdr` set it to a NUL-terminated string the loader keeps for the
        // object's life, or left it null.
        (!ptr.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    };
    Some((info.dli_fbase as u64, text(info.dli_fname), text(info.dli_sname), info.dli_saddr as u64))
}

#[cfg(not(unix))]
fn loaded(_pc: u64) -> Option<(u64, Option<String>, Option<String>, u64)> {
    None
}

/// Where the object holding `pc` is loaded.
fn image_base(pc: u64) -> Option<u64> {
    loaded(pc).map(|it| it.0)
}

/// The name the dynamic loader knows for `pc`: an exported symbol, else the object and the
/// offset in it. In this executable (loaded at `own`) it is always the object and offset:
/// `dladdr` sees only its exported symbols, and would name a function it has no symbol for by
/// whichever export comes before it.
fn shared(pc: u64, own: Option<u64>) -> String {
    let Some((base, file, sname, saddr)) = loaded(pc) else {
        return format!("{pc:#x}");
    };
    let sname = if own == Some(base) { None } else { sname };
    match (sname, file) {
        (Some(name), _) => format!("{}+{:#x}", demangle(&name), pc.wrapping_sub(saddr)),
        (None, Some(file)) => {
            let file = file.rsplit('/').next().unwrap_or(&file).to_owned();
            format!("{file}+{:#x}", pc.wrapping_sub(base))
        }
        (None, None) => format!("{pc:#x}"),
    }
}

fn shared_frame(pc: u64, own: Option<u64>) -> Frame {
    let mut f = Frame::named(shared(pc, own), FrameKind::Native);
    if let Some((base, file, sname, _)) = loaded(pc) {
        f.module = file;
        f.address = Some(pc.wrapping_sub(base));
        if own != Some(base)
            && let Some(name) = sname
        {
            f.function = demangle(&name);
            f.system_name = Some(name).filter(|it| *it != f.function);
        }
    } else {
        f.address = Some(pc);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn a_function_to_find() -> u64 {
        std::hint::black_box(a_function_to_find as fn() -> u64 as usize as u64)
    }

    #[test]
    fn an_address_in_a_function_is_named_by_it() {
        let pc = a_function_to_find() + 4;
        let names = names(&[pc]);
        assert!(names[&pc].contains("a_function_to_find"), "{}", names[&pc]);
        let frames = Symbolizer::new().symbolize(&[pc]).remove(0);
        assert!(frames.iter().any(|f| f.function.contains("a_function_to_find")), "{frames:?}");
    }

    /// Naming addresses again, by either path, reads the executable's symbol table no more:
    /// before 2026-11-03 each [`names`] call (each history flush of `blaze serve`) parsed it
    /// again, a quarter of an idle daemon's history thread.
    #[test]
    fn naming_addresses_again_reads_the_executable_once() {
        let pc = a_function_to_find() + 4;
        let first = names(&[pc]);
        assert!(first[&pc].contains("a_function_to_find"), "{}", first[&pc]);
        let parsed = parses();
        let again = names(&[pc, pc + 4]);
        _ = Symbolizer::new().symbolize(&[pc]);
        assert_eq!(again[&pc], first[&pc]);
        assert_eq!(parses(), parsed, "naming again parsed the executable's symbol table again");
        assert_eq!(parsed, 1, "the symbol table was parsed {parsed} times in this process");
    }

    /// An address is named only by a symbol of its own section: past `__text`'s end (a live
    /// image's stubs come next) it is not `__text`'s last function, however far on.
    #[test]
    fn an_address_past_its_sections_symbols_is_not_named_by_them() {
        let table = Table {
            path: String::new(),
            data: b"f",
            build_id: None,
            code: vec![0x1000..0x2000, 0x3000..0x4000],
            stubs: None,
            starts: vec![0x1000],
            rest: vec![(0, 0, 1)],
            demangled: Mutex::new(BTreeMap::new()),
        };
        assert_eq!(table.name(0x1010, 0).as_deref(), Some("f+0x10"));
        assert_eq!(table.name(0x3010, 0), None, "an address of the next code section");
        assert_eq!(table.name(0x2010, 0), None, "an address of no code section");
    }

    #[cfg(unix)]
    #[test]
    fn an_address_of_this_executable_the_loader_names_is_its_offset_not_an_export() {
        let pc = a_function_to_find() + 4;
        let own = image_base(pc);
        assert!(own.is_some(), "the loader knows this executable");
        let name = shared(pc, own);
        let exe = std::env::current_exe().unwrap();
        let file = exe.file_name().unwrap().to_string_lossy();
        assert_eq!(name, format!("{file}+{:#x}", pc - own.unwrap()));
    }
}
