//! What a page calls. Bytes go in through [`blaze_alloc`] and come out as a pointer and a
//! length; there is one compiler for the page.

use std::{cell::RefCell, mem::ManuallyDrop};

use crate::{Compiler, bundle};

#[derive(Default)]
struct Page {
    compiler: Compiler,
    /// The module of the last build, or nothing when it failed.
    module: Vec<u8>,
    /// The report of the last build as JSON, or why it failed as text.
    report: String,
}

thread_local! {
    static PAGE: RefCell<Page> = RefCell::default();
}

/// Memory for the page to write an argument to. The call that takes it frees it.
#[unsafe(no_mangle)]
pub extern "C" fn blaze_alloc(len: usize) -> *mut u8 {
    // A panic is a trap, and says nothing unless it says it first.
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let text = info.to_string();
            // SAFETY: the text lives through the call.
            unsafe { log(text.as_ptr(), text.len()) };
        }));
    });
    ManuallyDrop::new(Vec::<u8>::with_capacity(len)).as_mut_ptr()
}

/// Takes the bytes of an argument back from the page.
///
/// # Safety
///
/// `ptr` is from [`blaze_alloc`] called with `len`, and the page wrote all `len` bytes.
unsafe fn argument(ptr: *mut u8, len: usize) -> Vec<u8> {
    // SAFETY: an allocation of ours with that capacity, initialized by the page.
    unsafe { Vec::from_raw_parts(ptr, len, len) }
}

/// Adds the files of a bundle, see [`bundle`].
///
/// # Safety
///
/// As [`argument`] asks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_add_bundle(ptr: *mut u8, len: usize) -> bool {
    // SAFETY: the caller's promise.
    let bytes = unsafe { argument(ptr, len) };
    PAGE.with_borrow_mut(|page| match bundle::unpack(&bytes) {
        Ok(files) => {
            page.compiler.set_files(files);
            true
        }
        Err(e) => {
            page.report = e;
            false
        }
    })
}

/// Sets the workspace of a pack, see [`crate::pack`], as what there is to compile.
///
/// # Safety
///
/// As [`argument`] asks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_set_pack(ptr: *mut u8, len: usize) -> bool {
    // SAFETY: the caller's promise.
    let bytes = unsafe { argument(ptr, len) };
    PAGE.with_borrow_mut(|page| {
        let pack = serde_json::from_slice(&bytes).map_err(|e| e.to_string());
        drop(bytes);
        match pack.and_then(|pack| page.compiler.set_pack(pack, crate::macros::expand)) {
            Ok(()) => true,
            Err(e) => {
                page.report = e;
                false
            }
        }
    })
}

/// Resolves names and infers the program's types, leaving how long that took as the report.
#[unsafe(no_mangle)]
pub extern "C" fn blaze_analyze() {
    PAGE.with_borrow_mut(|page| {
        let (crates, names, types) = page.compiler.analyze();
        page.report = format!(
            "{crates} crates: names {names:.1?}, types of the program {types:.1?}, {} expansions missed",
            page.compiler.missed().len()
        );
    })
}

/// Says where in the memory and table of the module the program lives in it goes. Whether it
/// worked; if not, [`blaze_report`] says why.
#[unsafe(no_mangle)]
pub extern "C" fn blaze_set_start(table_start: u32, memory_start: u32) -> bool {
    PAGE.with_borrow_mut(|page| match page.compiler.set_start(table_start, memory_start.into()) {
        Ok(()) => true,
        Err(e) => {
            page.report = e;
            false
        }
    })
}

/// Sets the text of a file.
///
/// # Safety
///
/// As [`argument`] asks, for the path and for the text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_set_file(
    path: *mut u8,
    path_len: usize,
    text: *mut u8,
    text_len: usize,
) {
    // SAFETY: the caller's promise.
    let (path, text) = unsafe { (argument(path, path_len), argument(text, text_len)) };
    let (path, text) = (String::from_utf8_lossy(&path), String::from_utf8_lossy(&text));
    PAGE.with_borrow_mut(|page| page.compiler.set_file(&path, text.into_owned()));
}

#[link(wasm_import_module = "blaze")]
unsafe extern "C" {
    /// Shows text in the page's console.
    fn log(text: *const u8, len: usize);
}

/// Builds. Whether it worked says what [`blaze_report`] is: the build's JSON, or an error.
#[unsafe(no_mangle)]
pub extern "C" fn blaze_build() -> bool {
    PAGE.with_borrow_mut(|page| match page.compiler.build() {
        Ok(build) => {
            page.report = build.report();
            page.module = build.patch.module;
            true
        }
        Err(e) => {
            page.report = e;
            page.module.clear();
            false
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_module() -> *const u8 {
    PAGE.with_borrow(|page| page.module.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_module_len() -> usize {
    PAGE.with_borrow(|page| page.module.len())
}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_report() -> *const u8 {
    PAGE.with_borrow(|page| page.report.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_report_len() -> usize {
    PAGE.with_borrow(|page| page.report.len())
}
