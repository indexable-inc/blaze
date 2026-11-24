//! Zero-import, `#![no_std]`, `#![blaze::sans_io]` `wasm32-unknown-unknown` C-ABI cdylib over
//! [`blaze_site::BlazeSite`] and Rust-authored `gpu::slug` (`den` Slug + SDF) WebGPU shaders.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![blaze::trust(
    unsafe,
    reason = "wasm32 C-ABI pointer exports and freestanding per-handle linear-memory allocator"
)]
#![allow(
    unsafe_code,
    reason = "C-ABI raw pointer exports and freestanding wasm32 linear-memory allocator"
)]

pub mod host;

use core::alloc::{AllocError, Allocator, Layout};
use core::ptr::{self, NonNull};

use blaze_own::{Box as OwnBox, String as OwnString, Vec as OwnVec};
use blaze_site::{BlazeSite, ImageName, SiteInst, SiteUniform, emit_slug_site_wgsl};

#[cfg(all(target_arch = "wasm32", not(test)))]
#[panic_handler]
fn wasm_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

// =============================================================================
// Freestanding Per-Handle Segregated Free-List Allocator (Zero Statics)
// =============================================================================

const HEADER_BYTES: usize = 16;
const NUM_CLASSES: usize = 22; // 16B .. 32MiB
const MIN_CLASS_SHIFT: u32 = 4;
#[cfg(target_arch = "wasm32")]
const WASM_PAGE_BYTES: usize = 65_536;

#[repr(C)]
struct FreeNode {
    next: *mut FreeNode,
}

#[repr(C)]
struct ArenaState {
    free_lists: [*mut FreeNode; NUM_CLASSES],
    bump_ptr: usize,
    bump_end: usize,
}

#[derive(Clone, Copy)]
pub struct WasmAlloc(*mut ArenaState);

fn size_to_class(total: usize) -> Option<(usize, usize)> {
    let mut cls = 0usize;
    let mut cap = 1usize << MIN_CLASS_SHIFT;
    while cls < NUM_CLASSES {
        if cap >= total {
            return Some((cls, cap));
        }
        cls += 1;
        cap <<= 1;
    }
    None
}

fn align_up(addr: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    addr.checked_add(mask).map(|v| v & !mask)
}

fn grow_arena(st: &mut ArenaState, need: usize) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let pages = need.div_ceil(WASM_PAGE_BYTES).max(8);
        let prev = core::arch::wasm32::memory_grow(0, pages);
        if prev == usize::MAX {
            return false;
        }
        let base = prev.saturating_mul(WASM_PAGE_BYTES);
        let end = base.saturating_add(pages.saturating_mul(WASM_PAGE_BYTES));
        st.bump_ptr = base;
        st.bump_end = end;
        true
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (st, need);
        false
    }
}

fn new_arena_ptr() -> *mut ArenaState {
    #[cfg(target_arch = "wasm32")]
    {
        let pages = 32usize; // 2 MiB initial heap
        let prev = core::arch::wasm32::memory_grow(0, pages);
        if prev == usize::MAX {
            return ptr::null_mut();
        }
        let base = prev.saturating_mul(WASM_PAGE_BYTES);
        let end = base.saturating_add(pages.saturating_mul(WASM_PAGE_BYTES));
        let st_ptr: *mut ArenaState = ptr::with_exposed_provenance_mut(base);
        let first_free = match align_up(base.saturating_add(core::mem::size_of::<ArenaState>()), HEADER_BYTES) {
            Some(v) => v,
            None => return ptr::null_mut(),
        };
        unsafe {
            st_ptr.write(ArenaState {
                free_lists: [ptr::null_mut(); NUM_CLASSES],
                bump_ptr: first_free,
                bump_end: end,
            });
        }
        st_ptr
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        ptr::null_mut()
    }
}

unsafe impl Allocator for WasmAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.0.is_null() {
            return Err(AllocError);
        }
        let size = layout.size().max(1);
        let align = layout.align().max(HEADER_BYTES);
        let total = size.checked_add(align).ok_or(AllocError)?;
        let (cls, block_size) = size_to_class(total).ok_or(AllocError)?;

        let st = unsafe { &mut *self.0 };
        let head_slot = st.free_lists.get_mut(cls).ok_or(AllocError)?;
        let raw_block: *mut u8 = if !head_slot.is_null() {
            let node = *head_slot;
            *head_slot = unsafe { (*node).next };
            node.cast::<u8>()
        } else {
            let aligned_start = align_up(st.bump_ptr, HEADER_BYTES).ok_or(AllocError)?;
            let next_end = aligned_start.checked_add(block_size).ok_or(AllocError)?;
            if next_end > st.bump_end {
                if !grow_arena(st, block_size.saturating_add(HEADER_BYTES)) {
                    return Err(AllocError);
                }
                let start2 = align_up(st.bump_ptr, HEADER_BYTES).ok_or(AllocError)?;
                let end2 = start2.checked_add(block_size).ok_or(AllocError)?;
                if end2 > st.bump_end {
                    return Err(AllocError);
                }
                st.bump_ptr = end2;
                ptr::with_exposed_provenance_mut(start2)
            } else {
                st.bump_ptr = next_end;
                ptr::with_exposed_provenance_mut(aligned_start)
            }
        };

        let base_addr = raw_block.addr();
        let user_addr = align_up(base_addr.checked_add(HEADER_BYTES).ok_or(AllocError)?, align)
            .ok_or(AllocError)?;
        let header_ptr: *mut usize = ptr::with_exposed_provenance_mut(user_addr - HEADER_BYTES);
        unsafe {
            header_ptr.write(base_addr);
            header_ptr.add(1).write(cls);
        }
        let user_ptr: *mut u8 = ptr::with_exposed_provenance_mut(user_addr);
        let nn = NonNull::new(user_ptr).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(nn, size))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 || self.0.is_null() {
            return;
        }
        let user_addr = ptr.as_ptr().addr();
        if user_addr < HEADER_BYTES {
            return;
        }
        let header_ptr: *const usize = ptr::with_exposed_provenance(user_addr - HEADER_BYTES);
        let (base_addr, cls) = unsafe { (header_ptr.read(), header_ptr.add(1).read()) };
        let st = unsafe { &mut *self.0 };
        let Some(head_slot) = st.free_lists.get_mut(cls) else { return };
        let node: *mut FreeNode = ptr::with_exposed_provenance_mut(base_addr);
        unsafe {
            (*node).next = *head_slot;
        }
        *head_slot = node;
    }
}

// =============================================================================
// C-ABI Exports (Zero Imports, Zero Handwritten WGSL)
// =============================================================================

pub struct SiteHandle {
    site: BlazeSite<WasmAlloc>,
    wgsl: OwnString<WasmAlloc>,
    scratch: OwnVec<u8, WasmAlloc>,
    copy_buf: OwnString<WasmAlloc>,
    nav_buf: OwnString<WasmAlloc>,
    html_buf: OwnString<WasmAlloc>,
    og_rgba: OwnVec<u8, WasmAlloc>,
    projected_entities: u32,
}

fn with_handle<'a>(h: *mut SiteHandle) -> Option<&'a mut SiteHandle> {
    if h.is_null() { None } else { Some(unsafe { &mut *h }) }
}

/// Allocate a new [`BlazeSite`] engine and emit its Rust-authored Slug + SDF WebGPU WGSL shader.
#[unsafe(no_mangle)]
pub extern "C" fn site_create() -> *mut SiteHandle {
    let arena = new_arena_ptr();
    if arena.is_null() {
        return ptr::null_mut();
    }
    let alloc = WasmAlloc(arena);
    let Ok(site) = BlazeSite::new_in(alloc) else {
        return ptr::null_mut();
    };
    let mut wgsl = OwnString::new_in(alloc);
    if emit_slug_site_wgsl(&mut wgsl).is_err() {
        return ptr::null_mut();
    }
    let Ok(boxed) = OwnBox::try_new_in(
        SiteHandle {
            site,
            wgsl,
            scratch: OwnVec::new_in(alloc),
            copy_buf: OwnString::new_in(alloc),
            nav_buf: OwnString::new_in(alloc),
            html_buf: OwnString::new_in(alloc),
            og_rgba: OwnVec::new_in(alloc),
            projected_entities: 0,
        },
        alloc,
    ) else {
        return ptr::null_mut();
    };
    OwnBox::into_raw_with_allocator(boxed).0
}

/// Prepare a host-writable byte buffer of `len` bytes inside WASM linear memory (for `page.json` / `text.bin`).
#[unsafe(no_mangle)]
pub extern "C" fn site_scratch_alloc(h: *mut SiteHandle, len: usize) -> *mut u8 {
    let Some(st) = with_handle(h) else { return ptr::null_mut() };
    st.scratch.clear();
    if st.scratch.try_reserve(len).is_err() {
        return ptr::null_mut();
    }
    for _ in 0..len {
        if st.scratch.try_push(0).is_err() {
            return ptr::null_mut();
        }
    }
    st.scratch.as_mut_ptr()
}

/// Decode the `BZG1` Slug binary glyph table currently sitting in the scratch buffer.
#[unsafe(no_mangle)]
pub extern "C" fn site_load_font_table_from_scratch(h: *mut SiteHandle, len: usize) -> u32 {
    let Some(st) = with_handle(h) else { return 0 };
    let slice = match st.scratch.as_slice().get(..len) {
        Some(s) => s,
        None => return 0,
    };
    if st.site.load_font_table(slice).is_ok() { 1 } else { 0 }
}

/// Parse `page.<hash>.json` currently sitting in the scratch buffer and mount into ECS.
#[unsafe(no_mangle)]
pub extern "C" fn site_load_page_json_from_scratch(h: *mut SiteHandle, len: usize) -> u32 {
    let Some(st) = with_handle(h) else { return 0 };
    let slice = match st.scratch.as_slice().get(..len) {
        Some(s) => s,
        None => return 0,
    };
    if st.site.load_page_json(slice).is_ok() { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_wgsl_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.wgsl.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_wgsl_len(h: *mut SiteHandle) -> usize {
    match with_handle(h) {
        Some(st) => st.wgsl.len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_svg_count() -> usize {
    ImageName::COUNT
}

#[unsafe(no_mangle)]
pub extern "C" fn site_svg_ptr(idx: usize) -> *const u8 {
    match ImageName::ALL.get(idx) {
        Some(img) => img.svg_source().as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_svg_len(idx: usize) -> usize {
    match ImageName::ALL.get(idx) {
        Some(img) => img.svg_source().len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_svg_aspect(idx: usize) -> f32 {
    match ImageName::ALL.get(idx) {
        Some(img) => img.aspect(),
        None => 1.0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_set_svg_uv(h: *mut SiteHandle, idx: usize, u0: f32, v0: f32, uw: f32, vh: f32) {
    if let Some(st) = with_handle(h) {
        st.site.set_svg_uv(idx, u0, v0, uw, vh);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_curves_ptr(h: *mut SiteHandle) -> *const [f32; 4] {
    match with_handle(h) {
        Some(st) => st.site.table.curves.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_curves_len(h: *mut SiteHandle) -> usize {
    match with_handle(h) {
        Some(st) => st.site.table.curves.len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_bands_ptr(h: *mut SiteHandle) -> *const u32 {
    match with_handle(h) {
        Some(st) => st.site.table.bands.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_bands_len(h: *mut SiteHandle) -> usize {
    match with_handle(h) {
        Some(st) => st.site.table.bands.len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_uniform_ptr(h: *mut SiteHandle) -> *const SiteUniform {
    match with_handle(h) {
        Some(st) => &raw const st.site.uniform,
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_uniform_bytes() -> usize {
    core::mem::size_of::<SiteUniform>()
}

#[unsafe(no_mangle)]
pub extern "C" fn site_insts_ptr(h: *mut SiteHandle) -> *const SiteInst {
    match with_handle(h) {
        Some(st) => st.site.insts.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_insts_len(h: *mut SiteHandle) -> usize {
    match with_handle(h) {
        Some(st) => st.site.insts.len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_inst_stride() -> usize {
    core::mem::size_of::<SiteInst>()
}

#[unsafe(no_mangle)]
pub extern "C" fn site_render_frame(h: *mut SiteHandle, vp_w: f32, vp_h: f32, dpr: f32, now_ms: f64) -> u32 {
    let Some(st) = with_handle(h) else { return 0 };
    match st.site.render_frame(vp_w, vp_h, dpr, now_ms) {
        Ok(true) => 1,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_set_dark(h: *mut SiteHandle, dark: u32) {
    if let Some(st) = with_handle(h) {
        let _ = st.site.set_dark(dark != 0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_is_dark(h: *mut SiteHandle) -> u32 {
    match with_handle(h) {
        Some(st) if st.site.is_dark() => 1,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_bg_r(h: *mut SiteHandle) -> f32 {
    with_handle(h).map_or(0.063, |st| st.site.theme().bg.0[0])
}

#[unsafe(no_mangle)]
pub extern "C" fn site_bg_g(h: *mut SiteHandle) -> f32 {
    with_handle(h).map_or(0.063, |st| st.site.theme().bg.0[1])
}

#[unsafe(no_mangle)]
pub extern "C" fn site_bg_b(h: *mut SiteHandle) -> f32 {
    with_handle(h).map_or(0.067, |st| st.site.theme().bg.0[2])
}

#[unsafe(no_mangle)]
pub extern "C" fn site_pointer_move(h: *mut SiteHandle, sx: f32, sy: f32, now_ms: f64) {
    if let Some(st) = with_handle(h) {
        let _ = st.site.on_pointer_move(sx, sy, now_ms);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_pointer_down(h: *mut SiteHandle, sx: f32, sy: f32, now_ms: f64) {
    if let Some(st) = with_handle(h) {
        let _ = st.site.on_pointer_down(sx, sy, now_ms);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_pointer_up(h: *mut SiteHandle, sx: f32, sy: f32, now_ms: f64) {
    if let Some(st) = with_handle(h) {
        let _ = st.site.on_pointer_up(sx, sy, now_ms);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_wheel(h: *mut SiteHandle, dx: f32, dy: f32) {
    if let Some(st) = with_handle(h) {
        st.site.on_wheel(dx, dy);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_pinch(h: *mut SiteHandle, sx: f32, sy: f32, factor: f32) -> u32 {
    let Some(st) = with_handle(h) else { return 0 };
    if st.site.on_pinch(sx, sy, factor).unwrap_or(false) { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_reset_zoom(h: *mut SiteHandle) {
    if let Some(st) = with_handle(h) {
        let _ = st.site.reset_zoom();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_key_down(h: *mut SiteHandle, code: u32, shift: u32, mod_key: u32, now_ms: f64) -> u32 {
    let Some(st) = with_handle(h) else { return 0 };
    if st.site.on_key_down(code, shift != 0, mod_key != 0, now_ms).unwrap_or(false) { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_text_char(h: *mut SiteHandle, cp: u32, now_ms: f64) -> u32 {
    let Some(ch) = char::from_u32(cp) else { return 0 };
    let Some(st) = with_handle(h) else { return 0 };
    if st.site.on_text_char(ch, now_ms).unwrap_or(false) { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_zoom(h: *mut SiteHandle) -> f32 {
    with_handle(h).map_or(1.0, |st| st.site.zoom())
}

#[unsafe(no_mangle)]
pub extern "C" fn site_glyph_count(h: *mut SiteHandle) -> u32 {
    with_handle(h).map_or(0, |st| st.site.last_glyph_count)
}

#[unsafe(no_mangle)]
pub extern "C" fn site_frame_count(h: *mut SiteHandle) -> u32 {
    with_handle(h).map_or(0, |st| u32::try_from(st.site.frame_count & 0xffff_ffff).unwrap_or(0))
}

#[unsafe(no_mangle)]
pub extern "C" fn site_find_open(h: *mut SiteHandle) -> u32 {
    match with_handle(h) {
        Some(st) if st.site.find_open() => 1,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_find_matches(h: *mut SiteHandle) -> usize {
    with_handle(h).map_or(0, |st| st.site.find_match_count())
}

#[unsafe(no_mangle)]
pub extern "C" fn site_cursor_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.site.cursor().css_name().as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_cursor_len(h: *mut SiteHandle) -> usize {
    match with_handle(h) {
        Some(st) => st.site.cursor().css_name().len(),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_take_copy_len(h: *mut SiteHandle) -> usize {
    let Some(st) = with_handle(h) else { return 0 };
    match st.site.take_pending_copy(&mut st.copy_buf) {
        Ok(true) => st.copy_buf.len(),
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_copy_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.copy_buf.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_take_nav_len(h: *mut SiteHandle) -> usize {
    let Some(st) = with_handle(h) else { return 0 };
    match st.site.take_pending_nav(&mut st.nav_buf) {
        Ok(true) => st.nav_buf.len(),
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_nav_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.nav_buf.as_ptr(),
        None => ptr::null(),
    }
}

/// Project the live `blaze_ecs::World` scene graph into semantic HTML for non-WebGPU fallback.
#[unsafe(no_mangle)]
pub extern "C" fn site_project_html_len(h: *mut SiteHandle) -> usize {
    let Some(st) = with_handle(h) else { return 0 };
    match st.site.project_html(&mut st.html_buf) {
        Ok(n) => {
            st.projected_entities = n;
            st.html_buf.len()
        }
        Err(_) => {
            st.projected_entities = 0;
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_project_html_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.html_buf.as_ptr(),
        None => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_projected_entities(h: *mut SiteHandle) -> u32 {
    with_handle(h).map_or(0, |st| st.projected_entities)
}

#[unsafe(no_mangle)]
pub extern "C" fn site_og_width() -> u32 {
    blaze_site::OG_WIDTH
}

#[unsafe(no_mangle)]
pub extern "C" fn site_og_height() -> u32 {
    blaze_site::OG_HEIGHT
}

/// Render a `1200×630` Open Graph RGBA8 image directly from the ECS via Rust GPU `slug_site_fs`.
#[unsafe(no_mangle)]
pub extern "C" fn site_render_og_rgba_len(h: *mut SiteHandle) -> usize {
    let Some(st) = with_handle(h) else { return 0 };
    match st.site.render_og_rgba(&mut st.og_rgba) {
        Ok(_) => st.og_rgba.len(),
        Err(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn site_og_rgba_ptr(h: *mut SiteHandle) -> *const u8 {
    match with_handle(h) {
        Some(st) => st.og_rgba.as_ptr(),
        None => ptr::null(),
    }
}

#[cfg(test)]
mod tests {
    use super::host::SITE_HTML;

    #[test]
    fn host_html_contains_zero_handwritten_wgsl() {
        assert!(!SITE_HTML.contains("@vertex"));
        assert!(!SITE_HTML.contains("@fragment"));
        assert!(!SITE_HTML.contains("var<storage"));
        assert!(!SITE_HTML.contains("var<uniform"));
        assert!(SITE_HTML.contains("site_wgsl_ptr"));
        assert!(SITE_HTML.contains("site_pinch"));
        assert!(SITE_HTML.contains("slug_site_vs"));
        assert!(SITE_HTML.contains("slug_site_fs"));
        assert!(!SITE_HTML.contains("ctrlHeld"));
        assert!(SITE_HTML.contains("e.ctrlKey && !e.metaKey"));
        assert!(SITE_HTML.contains("site_project_html_len"));
    }
}
