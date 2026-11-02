//! Names for code addresses, found when a profile is written, never while sampling: the
//! executable's symbol table and DWARF (functions, inline chains, files and lines, through
//! `addr2line`), its load slide from the dynamic loader, and `dladdr` for code in shared
//! libraries. [`names`] is the short form blaze-profile's dumps use.

use std::{collections::BTreeMap, sync::Arc};

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

/// The executable, read once.
struct Image {
    path: String,
    build_id: Option<String>,
    /// Text symbols: (start, size or 0, mangled name), by start.
    symbols: Vec<(u64, u64, String)>,
    dwarf: Option<addr2line::Context<Reader>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Image {
    fn read(path: &std::path::Path) -> Option<Image> {
        use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
        let file = std::fs::File::open(path).ok()?;
        let cache = object::ReadCache::new(file);
        let object = object::File::parse(&cache).ok()?;
        let mut symbols: Vec<(u64, u64, String)> = object
            .symbols()
            .filter(|it| it.kind() == SymbolKind::Text && it.address() != 0)
            .filter_map(|it| Some((it.address(), it.size(), it.name().ok()?.to_owned())))
            .collect();
        symbols.sort();
        symbols.dedup_by_key(|it| it.0);
        let build_id = match object.build_id() {
            Ok(Some(id)) => Some(hex(id)),
            _ => object.mach_uuid().ok().flatten().map(|it| hex(&it)),
        };
        let endian =
            if object.is_little_endian() { gimli::RunTimeEndian::Little } else { gimli::RunTimeEndian::Big };
        let has_dwarf = object.section_by_name(".debug_info").is_some_and(|s| s.size() > 0);
        let mut dwarf = if has_dwarf { load_dwarf(&object, endian) } else { None };
        // macOS keeps DWARF beside the binary in a .dSYM bundle when dsymutil ran.
        if dwarf.is_none() && cfg!(target_vendor = "apple") {
            let name = path.file_name()?.to_owned();
            let mut dsym = path.as_os_str().to_owned();
            dsym.push(".dSYM");
            let dsym = std::path::PathBuf::from(dsym).join("Contents/Resources/DWARF").join(name);
            if let Ok(file) = std::fs::File::open(&dsym) {
                let cache = object::ReadCache::new(file);
                if let Ok(object) = object::File::parse(&cache) {
                    dwarf = load_dwarf(&object, endian);
                }
            }
        }
        Some(Image { path: path.to_string_lossy().into_owned(), build_id, symbols, dwarf })
    }

    /// The symbol `at` (a file address) falls in, and its start.
    fn symbol(&self, at: u64) -> Option<&(u64, u64, String)> {
        let found = self.symbols.partition_point(|it| it.0 <= at);
        let it = self.symbols.get(found.checked_sub(1)?)?;
        // Past the next function's start the address is not this one's; Mach-O gives no sizes.
        if self.symbols.get(found).is_some_and(|next| at >= next.0) || (it.1 != 0 && at >= it.0 + it.1) {
            return None;
        }
        Some(it)
    }

    /// `at`'s frames, outermost (the real function) first.
    fn frames(&self, at: u64) -> Vec<Frame> {
        let symbol = self.symbol(at);
        let base = |function: String| {
            let mut f = Frame::named(function, FrameKind::Native);
            f.module = Some(self.path.clone());
            f.build_id = self.build_id.clone();
            f.address = Some(at);
            f
        };
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
                    .or_else(|| symbol.map(|it| demangle(&it.2)))
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
            out.push(base(symbol.map_or_else(|| format!("{at:#x}"), |it| demangle(&it.2))));
        }
        // addr2line gives the innermost first.
        out.reverse();
        let depth = out.len();
        for (i, f) in out.iter_mut().enumerate() {
            f.inline_depth = i as u16;
            if i == 0 {
                f.system_name = symbol.map(|it| it.2.clone()).filter(|it| *it != f.function);
            }
        }
        debug_assert_eq!(depth, out.len());
        out
    }
}

/// Symbolizes addresses of this process.
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
        Symbolizer {
            image: None,
            slide: slide(),
            own_base: image_base(image_base as fn(u64) -> Option<u64> as usize as u64),
            cache: BTreeMap::new(),
        }
    }

    fn image(&mut self) -> Option<&Image> {
        self.image
            .get_or_insert_with(|| std::env::current_exe().ok().and_then(|it| Image::read(&it)))
            .as_ref()
    }

    fn lookup(&mut self, pc: u64) -> Vec<Frame> {
        let own = self.own_base;
        let in_own = loaded(pc).is_none_or(|info| Some(info.0) == own);
        if in_own
            && let Some(slide) = self.slide
            && let Some(at) = pc.checked_sub(slide)
            && let Some(image) = self.image()
        {
            let frames = image.frames(at);
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
pub fn names(pcs: &[u64]) -> BTreeMap<u64, String> {
    let mut sym = Symbolizer::new();
    let mut out = BTreeMap::new();
    let mut pcs = pcs.to_vec();
    pcs.sort_unstable();
    pcs.dedup();
    for pc in pcs {
        let own = sym.own_base;
        let in_own = loaded(pc).is_none_or(|info| Some(info.0) == own);
        let name = match (in_own, sym.slide) {
            (true, Some(slide)) => pc
                .checked_sub(slide)
                .and_then(|at| {
                    let image = sym.image()?;
                    let (start, _, name) = image.symbol(at)?;
                    Some(format!("{}+{:#x}", demangle(name), at - start))
                }),
            _ => None,
        };
        out.insert(pc, name.unwrap_or_else(|| shared(pc, own)));
    }
    out
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
