//! The renderer behind a C ABI, for any wasm host (a Cloudflare Worker, wasmtime, a browser). No imports and no
//! globals: the host holds a handle, writes inputs into memory it got from `site_alloc`, and reads each result from
//! the handle's output buffer.
//!
//! ```text
//! h = site_new()
//! p = site_alloc(n); write snapshot at p; site_load(h, p, n); site_free(p, n)     -> 0, or 1 with the error as output
//! site_pages(h)                                                                   -> 0, output = JSON [{url, file}]
//! site_render(h, url, url_len)                                                    -> 0 with HTML, or 1 with the error
//! read site_output_len(h) bytes at site_output_ptr(h); site_drop(h)
//! ```

use site_render::Site;

pub struct Handle {
    site: Option<Site>,
    out: Vec<u8>,
}

impl Handle {
    fn status(&mut self, r: Result<Vec<u8>, String>) -> i32 {
        match r {
            Ok(out) => {
                self.out = out;
                0
            }
            Err(e) => {
                self.out = e.into_bytes();
                1
            }
        }
    }
}

/// # Safety
/// `ptr..ptr+len` must be readable (or `len` zero).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// # Safety
/// `ptr` and `len` from one `site_alloc`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_free(ptr: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

#[unsafe(no_mangle)]
pub extern "C" fn site_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle { site: None, out: Vec::new() }))
}

/// # Safety
/// `h` from `site_new`, dropped once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_drop(h: *mut Handle) {
    drop(unsafe { Box::from_raw(h) });
}

/// # Safety
/// `h` from `site_new`; the snapshot bytes readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_load(h: *mut Handle, ptr: *const u8, len: usize) -> i32 {
    let h = unsafe { &mut *h };
    let r = Site::from_snapshot(unsafe { bytes(ptr, len) }).map(|s| {
        h.site = Some(s);
        Vec::new()
    });
    h.status(r)
}

/// # Safety
/// `h` from `site_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_pages(h: *mut Handle) -> i32 {
    let h = unsafe { &mut *h };
    let r = h.site.as_ref().ok_or_else(|| "no site loaded".to_string()).map(|s| {
        let pages: Vec<_> = s
            .pages()
            .iter()
            .map(|p| serde_json::json!({ "url": p.url, "file": p.file }))
            .collect();
        serde_json::to_vec(&pages).expect("serialises")
    });
    h.status(r)
}

/// # Safety
/// `h` from `site_new`; the url readable for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_render(h: *mut Handle, url: *const u8, url_len: usize) -> i32 {
    let h = unsafe { &mut *h };
    let url = unsafe { bytes(url, url_len) };
    let r = match (h.site.as_ref(), std::str::from_utf8(url)) {
        (None, _) => Err("no site loaded".to_string()),
        (_, Err(e)) => Err(format!("url: {e}")),
        (Some(s), Ok(url)) => s.render(url).map(String::into_bytes),
    };
    h.status(r)
}

/// # Safety
/// `h` from `site_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_output_ptr(h: *const Handle) -> *const u8 {
    unsafe { (*h).out.as_ptr() }
}

/// # Safety
/// `h` from `site_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_output_len(h: *const Handle) -> usize {
    unsafe { (*h).out.len() }
}
