//! Names for the addresses CPU samples hold, found when a dump is written, never while
//! sampling: the executable's own symbol table (read through a cache, so a binary of hundreds
//! of MB is not read whole), its load slide from the dynamic loader, and `dladdr` for code in
//! shared libraries.

use blaze_hash::HashMap;

/// A name for each of `pcs`: the function demangled with its offset, or the library and offset,
/// or the bare address.
pub(crate) fn names(pcs: &[u64]) -> HashMap<u64, String> {
    let mut names = HashMap::default();
    let mut left: Vec<u64> = pcs.to_vec(); // alloc: a dump's addresses, on demand
    left.sort_unstable();
    left.dedup();
    if let Some(slide) = slide() {
        executable(&left, slide, &mut names);
    }
    let own = image_base(image_base as fn(u64) -> Option<u64> as usize as u64);
    for pc in left {
        names.entry(pc).or_insert_with(|| shared(pc, own));
    }
    names
}

fn demangled(name: &str) -> String {
    match rustc_demangle::try_demangle(name) {
        // alloc: naming samples, when a dump is written
        Ok(it) => format!("{it:#}"),
        Err(_) => {
            // alloc: naming samples, when a dump is written
            name.strip_prefix('_').filter(|_| cfg!(target_os = "macos")).unwrap_or(name).to_owned()
        }
    }
}

/// Names `pcs` (sorted) that fall in the executable's text symbols, loaded `slide` bytes from
/// the addresses the file gives.
fn executable(pcs: &[u64], slide: u64, names: &mut HashMap<u64, String>) {
    use object::{Object, ObjectSymbol, SymbolKind};
    let Ok(path) = std::env::current_exe() else { return };
    let Ok(file) = std::fs::File::open(&path) else { return };
    let cache = object::ReadCache::new(file);
    let Ok(object) = object::File::parse(&cache) else { return };
    // alloc: a dump's index of the executable's functions, on demand
    let mut starts: Vec<(u64, object::SymbolIndex)> = object
        .symbols()
        .filter(|it| it.kind() == SymbolKind::Text && it.address() != 0)
        .map(|it| (it.address(), it.index()))
        // alloc: naming samples, when a dump is written
        .collect();
    starts.sort_unstable_by_key(|it| it.0);
    for &pc in pcs {
        let Some(at) = pc.checked_sub(slide) else { continue };
        let found = starts.partition_point(|it| it.0 <= at);
        let Some(&(start, index)) = found.checked_sub(1).and_then(|it| starts.get(it)) else {
            continue;
        };
        // Past the next function's start the address is not this one's; Mach-O gives no sizes.
        if starts.get(found).is_some_and(|next| at >= next.0) {
            continue;
        }
        let Ok(symbol) = object.symbol_by_index(index) else { continue };
        if symbol.size() != 0 && at >= start + symbol.size() {
            continue;
        }
        let Ok(name) = symbol.name() else { continue };
        // alloc: naming samples, when a dump is written
        names.insert(pc, format!("{}+{:#x}", demangled(name), at - start));
    }
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

#[cfg(unix)]
fn loaded(pc: u64) -> Option<libc::Dl_info> {
    let mut info = libc::Dl_info {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a `Dl_info` to fill; `dladdr` only reads the loader's tables.
    (unsafe { libc::dladdr(pc as *const libc::c_void, &mut info) } != 0).then_some(info)
}

/// Where the object holding `pc` is loaded.
#[cfg(unix)]
fn image_base(pc: u64) -> Option<u64> {
    loaded(pc).map(|it| it.dli_fbase as u64)
}

#[cfg(not(unix))]
fn image_base(_pc: u64) -> Option<u64> {
    None
}

/// The name the dynamic loader knows for `pc`: an exported symbol, else the object and the
/// offset in it. In the object loaded at `own` (this executable, whose symbol table
/// [`executable`] already read) it is always the object and offset: `dladdr` sees only its
/// exported symbols, and would name a function it has no symbol for by whichever export comes
/// before it (a compiler builtin such as `__floattisf`).
#[cfg(unix)]
fn shared(pc: u64, own: Option<u64>) -> String {
    let Some(info) = loaded(pc) else {
        // alloc: naming samples, when a dump is written
        return format!("{pc:#x}");
    };
    let text = |ptr: *const libc::c_char| {
        // SAFETY: `dladdr` set it to a NUL-terminated string the loader keeps for the
        // object's life, or left it null.
        (!ptr.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy())
    };
    let sname = if own == Some(info.dli_fbase as u64) { None } else { text(info.dli_sname) };
    match (sname, text(info.dli_fname)) {
        (Some(name), _) => {
            // alloc: naming samples, when a dump is written
            format!("{}+{:#x}", demangled(&name), pc.wrapping_sub(info.dli_saddr as u64))
        }
        (None, Some(file)) => {
            // alloc: naming samples, when a dump is written
            let file = file.rsplit('/').next().unwrap_or(&file).to_owned();
            // alloc: naming samples, when a dump is written
            format!("{file}+{:#x}", pc.wrapping_sub(info.dli_fbase as u64))
        }
        // alloc: naming samples, when a dump is written
        (None, None) => format!("{pc:#x}"),
    }
}

#[cfg(not(unix))]
fn shared(pc: u64, _own: Option<u64>) -> String {
    // alloc: naming samples, when a dump is written
    format!("{pc:#x}")
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
        let name = &names[&pc];
        assert!(name.contains("a_function_to_find"), "{name}");
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
