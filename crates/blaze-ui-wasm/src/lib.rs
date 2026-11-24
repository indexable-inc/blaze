//! Zero-import, `#![no_std]`, `#![blaze::sans_io]` `wasm32-unknown-unknown` C-ABI cdylib over
//! [`blaze_ui::WorkbenchApp`] and Rust-authored `gpu::emit` WebGPU shaders.

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

use blaze_own::{Box as OwnBox, String as OwnString};
use blaze_ui::{
    ClipRectGpu, GlyphInst, GpuPatch, InputEvent, Key, RectInst, WorkbenchApp, emit_ui_glyph_wgsl,
    emit_ui_sdf_wgsl,
};

#[cfg(all(target_arch = "wasm32", not(test)))]
#[panic_handler]
fn wasm_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

// =============================================================================
// Freestanding Per-Handle Segregated Free-List Allocator (Zero Statics)
// =============================================================================

const HEADER_BYTES: usize = 16;
const NUM_CLASSES: usize = 20; // 16B .. 8MiB
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

/// Copy-able handle to a per-instance freestanding WASM linear-memory arena.
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
        let pages = need.div_ceil(WASM_PAGE_BYTES).max(4);
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
        let pages = 12usize;
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
// C-ABI Exports (Zero Imports, Zero Process Statics)
// =============================================================================

/// Opaque WASM UI Workbench handle owning its arena and relational ECS [`WorkbenchApp`].
pub struct Handle {
    alloc: WasmAlloc,
    app: WorkbenchApp<WasmAlloc>,
    out_buf: OwnString<WasmAlloc>,
}

/// Create a new Blaze UI Workbench handle inside a fresh per-handle WASM linear-memory arena.
#[unsafe(no_mangle)]
pub extern "C" fn ui_new(advance_ratio: f32) -> *mut Handle {
    let arena = new_arena_ptr();
    if arena.is_null() {
        return ptr::null_mut();
    }
    let alloc = WasmAlloc(arena);
    let Ok(mut app) = WorkbenchApp::new_in(alloc) else {
        return ptr::null_mut();
    };
    if advance_ratio.is_finite() && advance_ratio > 0.25 && advance_ratio < 1.2 {
        app.ui.draw.advance_ratio = advance_ratio;
    }
    let boxed = OwnBox::try_new_in(
        Handle { alloc, app, out_buf: OwnString::new_in(alloc) },
        alloc,
    );
    match boxed {
        Ok(b) => OwnBox::into_raw_with_allocator(b).0,
        Err(_) => ptr::null_mut(),
    }
}

/// Destroy a handle returned by [`ui_new`].
///
/// # Safety
/// `h` must be from `ui_new` and dropped at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_drop(h: *mut Handle) {
    if h.is_null() {
        return;
    }
    let alloc = unsafe { (*h).alloc };
    drop(unsafe { OwnBox::from_raw_in(h, alloc) });
}

/// Emit Rust-authored WebGPU SDF UI shader (`ui_sdf_vs` + `ui_sdf_fs`) into the output buffer.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_wgsl_sdf(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    handle.out_buf.clear();
    let _ = emit_ui_sdf_wgsl(&mut handle.out_buf);
    handle.out_buf.len()
}

/// Emit Rust-authored WebGPU Glyph Atlas UI shader (`ui_glyph_vs` + `ui_glyph_fs`) into output buffer.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_wgsl_glyph(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    handle.out_buf.clear();
    let _ = emit_ui_glyph_wgsl(&mut handle.out_buf);
    handle.out_buf.len()
}

/// Pointer to UTF-8 output buffer written by `ui_wgsl_sdf` / `ui_wgsl_glyph`.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_out_ptr(h: *const Handle) -> *const u8 {
    if h.is_null() { ptr::null() } else { unsafe { (*h).out_buf.as_ptr() } }
}

/// Forward pointer move event (`CSS px`).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_pointer_move(h: *mut Handle, x: f32, y: f32) {
    if !h.is_null() {
        let _ = unsafe { (*h).app.handle_input(InputEvent::PointerMove { x, y }) };
    }
}

/// Forward pointer down event.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_pointer_down(
    h: *mut Handle,
    x: f32,
    y: f32,
    button: u32,
    shift: u32,
) {
    if !h.is_null() {
        let b = u8::try_from(button).unwrap_or(0);
        let _ = unsafe {
            (*h).app
                .handle_input(InputEvent::PointerDown { x, y, button: b, shift: shift != 0 })
        };
    }
}

/// Forward pointer up event.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_pointer_up(h: *mut Handle, x: f32, y: f32, button: u32) {
    if !h.is_null() {
        let b = u8::try_from(button).unwrap_or(0);
        let _ = unsafe { (*h).app.handle_input(InputEvent::PointerUp { x, y, button: b }) };
    }
}

/// Forward double-click event.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_dblclick(h: *mut Handle, x: f32, y: f32) {
    if !h.is_null() {
        let _ = unsafe { (*h).app.handle_input(InputEvent::DoubleClick { x, y }) };
    }
}

/// Forward mouse wheel event.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_wheel(
    h: *mut Handle,
    x: f32,
    y: f32,
    dx: f32,
    dy: f32,
    ctrl_or_meta: u32,
) {
    if !h.is_null() {
        let _ = unsafe {
            (*h).app.handle_input(InputEvent::Wheel {
                x,
                y,
                dx,
                dy,
                ctrl_or_meta: ctrl_or_meta != 0,
            })
        };
    }
}

/// Forward special key event (`1=Esc, 2=Enter, 3=Tab, 4=Backspace, 10=Space`).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_key(h: *mut Handle, code: u32, shift: u32, ctrl_or_meta: u32) {
    if h.is_null() {
        return;
    }
    let key = match code {
        1 => Key::Escape,
        2 => Key::Enter,
        3 => Key::Tab,
        4 => Key::Backspace,
        10 => Key::Space,
        _ => return,
    };
    let _ = unsafe {
        (*h).app.handle_input(InputEvent::KeyDown {
            key,
            shift: shift != 0,
            ctrl_or_meta: ctrl_or_meta != 0,
        })
    };
}

/// Forward UTF-32 character input.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_on_char(h: *mut Handle, codepoint: u32) {
    if !h.is_null() {
        if let Some(ch) = char::from_u32(codepoint) {
            let _ = unsafe { (*h).app.handle_input(InputEvent::TextChar(ch)) };
        }
    }
}

/// Layout and extract one frame into GPU instance buffers.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_render_frame(
    h: *mut Handle,
    width: f32,
    height: f32,
    dpr: f32,
    time: f32,
) {
    if !h.is_null() {
        let _ = unsafe { (*h).app.render_frame(width, height, dpr, time) };
    }
}

/// Pointer to `ClipRectGpu` array (`32B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_clips_ptr(h: *const Handle) -> *const ClipRectGpu {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.clips.as_ptr() } }
}

/// Number of `ClipRectGpu` entries.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_clips_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.clips.len() } }
}

/// Pointer to base-layer `RectInst` array (`80B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_rects_ptr(h: *const Handle) -> *const RectInst {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.rects.as_ptr() } }
}

/// Number of base-layer `RectInst` instances.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_rects_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.rects.len() } }
}

/// Pointer to base-layer `GlyphInst` array (`64B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_glyphs_ptr(h: *const Handle) -> *const GlyphInst {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.glyphs.as_ptr() } }
}

/// Number of base-layer `GlyphInst` instances.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_glyphs_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.glyphs.len() } }
}

/// Pointer to overlay-layer `RectInst` array (`80B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_ov_rects_ptr(h: *const Handle) -> *const RectInst {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.overlay_rects.as_ptr() } }
}

/// Number of overlay-layer `RectInst` instances.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_ov_rects_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.overlay_rects.len() } }
}

/// Pointer to overlay-layer `GlyphInst` array (`64B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_ov_glyphs_ptr(h: *const Handle) -> *const GlyphInst {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.overlay_glyphs.as_ptr() } }
}

/// Number of overlay-layer `GlyphInst` instances.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_ov_glyphs_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.overlay_glyphs.len() } }
}

/// Pointer to current CSS cursor name (`"default"`, `"pointer"`, `"col-resize"`, etc.).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_cursor_ptr(h: *const Handle) -> *const u8 {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.cursor().css_name().as_ptr() } }
}

/// Byte length of current CSS cursor name.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_cursor_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.cursor().css_name().len() } }
}

/// Returns `1` if the host should upload full GPU buffers this frame, `0` for incremental patches.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_is_full_upload(h: *const Handle) -> u32 {
    if h.is_null() { 1 } else if unsafe { (*h).app.ui.draw.full_upload } { 1 } else { 0 }
}

/// Pointer to base-layer `GpuPatch` array for `rects` (`8B` stride: `[start, count]`).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_rect_patches_ptr(h: *const Handle) -> *const GpuPatch {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.rect_patches.as_ptr() } }
}

/// Number of base-layer rect patches.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_rect_patches_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.rect_patches.len() } }
}

/// Pointer to base-layer `GpuPatch` array for `glyphs` (`8B` stride).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_glyph_patches_ptr(h: *const Handle) -> *const GpuPatch {
    if h.is_null() { ptr::null() } else { unsafe { (*h).app.ui.draw.glyph_patches.as_ptr() } }
}

/// Number of base-layer glyph patches.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_glyph_patches_len(h: *const Handle) -> usize {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.draw.glyph_patches.len() } }
}

/// Number of layout nodes visited in the last frame (`0` on paint-only / idle frames).
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_last_layout_nodes(h: *const Handle) -> u32 {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.last_stats.layout_nodes_visited } }
}

/// Number of paint nodes extracted/patched in the last frame.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_last_paint_nodes(h: *const Handle) -> u32 {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.last_stats.paint_nodes_visited } }
}

/// GPU bytes patched in the last frame.
///
/// # Safety
/// `h` from `ui_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_last_patched_bytes(h: *const Handle) -> u32 {
    if h.is_null() { 0 } else { unsafe { (*h).app.ui.last_stats.gpu_bytes_patched } }
}

#[cfg(test)]
mod tests {
    use super::host::WORKBENCH_HTML;

    #[test]
    fn host_html_contains_zero_handwritten_wgsl() {
        assert!(!WORKBENCH_HTML.contains("@vertex"));
        assert!(!WORKBENCH_HTML.contains("@fragment"));
        assert!(WORKBENCH_HTML.contains("ui_wgsl_sdf"));
        assert!(WORKBENCH_HTML.contains("ui_wgsl_glyph"));
    }
}
