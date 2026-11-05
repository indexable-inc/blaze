//! The renderer behind a C ABI, for any wasm host (a Cloudflare Worker, wasmtime, a browser). No imports and no
//! globals: the host holds a handle, writes inputs into memory it got from `site_alloc`, and reads each result from
//! the handle's output buffer.
//!
//! ```text
//! h = site_new()
//! p = site_alloc(n); write snapshot at p; site_load(h, p, n); site_free(p, n)     -> 0, or 1 with the error as output
//! site_pages(h)                                                                   -> 0, output = JSON [{url, file, doc, discussable}]
//! site_discussable(h)                                                             -> 0, output = JSON {doc: {title, blocks: {id: text}}}
//! site_render(h, url, url_len, comments, comments_len, discussion, discussion_len) -> 0 with HTML, or 1 with the error
//! read site_output_len(h) bytes at site_output_ptr(h); site_drop(h)
//! ```
//!
//! `comments` is a CBOR list of `{id, block, quote, body, created_at, user_id, login, name, avatar_url}` and
//! `discussion` a CBOR map `{viewer: null | {id, login, admin}, sign_in, error: null | {block, message}}`; empty means
//! none and anonymous.

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
            .map(|p| serde_json::json!({ "url": p.url, "file": p.file, "doc": p.doc, "discussable": s.is_discussable(p) }))
            .collect();
        serde_json::to_vec(&pages).expect("serialises")
    });
    h.status(r)
}

/// Each discussable page's doc, title and blocks (what comments may attach to, with their text), in page order.
///
/// # Safety
/// `h` from `site_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_discussable(h: *mut Handle) -> i32 {
    let h = unsafe { &mut *h };
    let r = h.site.as_ref().ok_or_else(|| "no site loaded".to_string()).map(|s| {
        let mut m = serde_json::Map::new();
        for b in s.discussable() {
            let blocks: serde_json::Map<String, serde_json::Value> = b
                .blocks
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            m.insert(b.doc.clone(), serde_json::json!({ "title": b.title, "blocks": blocks }));
        }
        serde_json::to_vec(&m).expect("serialises")
    });
    h.status(r)
}

/// # Safety
/// `h` from `site_new`; each input readable for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn site_render(
    h: *mut Handle,
    url: *const u8,
    url_len: usize,
    comments: *const u8,
    comments_len: usize,
    discussion: *const u8,
    discussion_len: usize,
) -> i32 {
    let h = unsafe { &mut *h };
    let (url, comments, discussion) = unsafe {
        (bytes(url, url_len), bytes(comments, comments_len), bytes(discussion, discussion_len))
    };
    let r = match (h.site.as_mut(), std::str::from_utf8(url)) {
        (None, _) => Err("no site loaded".to_string()),
        (_, Err(e)) => Err(format!("url: {e}")),
        (Some(s), Ok(url)) => s.render_cbor(url, comments, discussion).map(|r| r.html.into_bytes()),
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
