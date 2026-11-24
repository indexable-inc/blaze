//! Zero-import, `#![no_std]`, `#![blaze::sans_io]` `wasm32-unknown-unknown` C-ABI wrapper over
//! [`blaze_ecs::ui::ExplorerApp`].
//!
//! All relational ECS state (`ChildOf` / `IsA` pairs, `GraphNode`, `SpatialBody`), compact
//! Phosphor-icon HTML rendering, Cmd+F matching, Rust-to-WGSL shader emission (`gpu::emit::emit_wgsl`),
//! and WebGPU node/edge instance buffer packing run inside WASM linear memory with zero `std`,
//! zero `alloc` crate, zero process statics, and zero JS dependencies.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![blaze::trust(
    unsafe,
    reason = "wasm32 C-ABI boundary and freestanding linear-memory allocator over wasm32::memory_grow"
)]
#![allow(
    unsafe_code,
    reason = "C-ABI raw pointer exports and freestanding wasm32 linear-memory allocator"
)]

pub mod shaders;

use core::alloc::{AllocError, Allocator, Layout};
use core::ptr::{self, NonNull};

use blaze_ecs::ui::ExplorerApp;
use blaze_own::{Box as OwnBox, String as OwnString};

#[cfg(all(target_arch = "wasm32", not(test)))]
#[panic_handler]
fn wasm_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

// =============================================================================
// Freestanding `#![no_std]` Per-Handle Segregated Free-List Allocator (Zero Statics)
// =============================================================================

const HEADER_BYTES: usize = 16;
const NUM_CLASSES: usize = 20; // 16B .. 8MiB power-of-two size classes
const MIN_CLASS_SHIFT: u32 = 4; // 16 bytes
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

/// Copy-able handle to a per-instance freestanding WASM linear-memory arena (`zero statics`).
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
        let pages = 8usize;
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
        // SAFETY: freshly grown wasm32 linear memory region `[base, end)`.
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

// SAFETY: returns properly aligned, uniquely owned blocks from linear memory and recycles them
// into power-of-two size-class free lists.
unsafe impl Allocator for WasmAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.0.is_null() {
            return Err(AllocError);
        }
        let size = layout.size().max(1);
        let align = layout.align().max(HEADER_BYTES);
        let total = size.checked_add(align).ok_or(AllocError)?;
        let (cls, block_size) = size_to_class(total).ok_or(AllocError)?;

        // SAFETY: single-threaded wasm32 guest linear memory owned by Handle's arena.
        let st = unsafe { &mut *self.0 };
        let head_slot = st.free_lists.get_mut(cls).ok_or(AllocError)?;
        let raw_block: *mut u8 = if !head_slot.is_null() {
            let node = *head_slot;
            // SAFETY: node was placed onto free_lists[cls] by deallocate.
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
        // SAFETY: HEADER_BYTES precede user_addr inside the allocated block.
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
        // SAFETY: written by `allocate` right before `ptr`.
        let (base_addr, cls) = unsafe { (header_ptr.read(), header_ptr.add(1).read()) };
        // SAFETY: single-threaded wasm32 guest; raw block is >= 16 bytes aligned for FreeNode.
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
// C-ABI Exports (Zero Imports, Zero wasm-bindgen, Zero Process Statics)
// =============================================================================

/// Opaque WASM Explorer handle owning its arena and client-side relational [`blaze_ecs::World`].
pub struct Handle {
    alloc: WasmAlloc,
    app: ExplorerApp<WasmAlloc>,
    wgsl_buf: OwnString<WasmAlloc>,
    out_ptr: *const u8,
    out_len: usize,
}

unsafe fn slice_from_raw<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        // SAFETY: caller allocated `ptr..ptr+len` via `explorer_alloc`.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

/// Create a new Explorer handle backed by a fresh per-handle WASM arena and relational `World`.
#[unsafe(no_mangle)]
pub extern "C" fn explorer_new() -> *mut Handle {
    let arena = new_arena_ptr();
    if arena.is_null() {
        return ptr::null_mut();
    }
    let alloc = WasmAlloc(arena);
    let Ok(app) = ExplorerApp::new_in(alloc) else {
        return ptr::null_mut();
    };
    let boxed = OwnBox::try_new_in(
        Handle {
            alloc,
            app,
            wgsl_buf: OwnString::new_in(alloc),
            out_ptr: ptr::null(),
            out_len: 0,
        },
        alloc,
    );
    match boxed {
        Ok(b) => OwnBox::into_raw_with_allocator(b).0,
        Err(_) => ptr::null_mut(),
    }
}

/// Allocate `len` bytes inside `h`'s WASM arena for host-to-guest inputs.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_alloc(h: *mut Handle, len: usize) -> *mut u8 {
    if h.is_null() {
        return ptr::null_mut();
    }
    let alloc = unsafe { (*h).alloc };
    let Ok(layout) = Layout::from_size_align(len.max(1), 8) else {
        return ptr::null_mut();
    };
    match alloc.allocate(layout) {
        Ok(nn) => nn.as_ptr().cast::<u8>(),
        Err(_) => ptr::null_mut(),
    }
}

/// Free a buffer previously returned by [`explorer_alloc`].
///
/// # Safety
/// `h` from `explorer_new`; `ptr` and `len` must match a single prior `explorer_alloc(h, len)` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_free(h: *mut Handle, ptr: *mut u8, len: usize) {
    if h.is_null() {
        return;
    }
    let Some(nn) = NonNull::new(ptr) else { return };
    let alloc = unsafe { (*h).alloc };
    if let Ok(layout) = Layout::from_size_align(len.max(1), 8) {
        // SAFETY: matches `explorer_alloc`.
        unsafe { alloc.deallocate(nn, layout) };
    }
}

/// Destroy an Explorer handle created by [`explorer_new`].
///
/// # Safety
/// `h` must be from `explorer_new` and dropped at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_drop(h: *mut Handle) {
    if h.is_null() {
        return;
    }
    let alloc = unsafe { (*h).alloc };
    // SAFETY: `h` was created by `OwnBox::into_raw_with_allocator` with `alloc`.
    drop(unsafe { OwnBox::from_raw_in(h, alloc) });
}

/// Load a `/api/world` JSON snapshot into the client-side relational ECS. Returns `1` on success.
///
/// # Safety
/// `h` from `explorer_new`; `ptr..ptr+len` readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_load_world(h: *mut Handle, ptr: *const u8, len: usize) -> i32 {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let bytes = unsafe { slice_from_raw(ptr, len) };
    if handle.app.load_world_json(bytes) { 1 } else { 0 }
}

/// Apply a JSON UI action to the client-side relational ECS. Returns `1` if handled.
///
/// # Safety
/// `h` from `explorer_new`; `ptr..ptr+len` readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_action(h: *mut Handle, ptr: *const u8, len: usize) -> i32 {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let bytes = unsafe { slice_from_raw(ptr, len) };
    if handle.app.apply_action_json(bytes) { 1 } else { 0 }
}

/// Render sidebar HTML into the handle's output buffer, returning its byte length.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_render_sidebar(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let ptr_len = {
        let s = handle.app.render_sidebar();
        (s.as_ptr(), s.len())
    };
    handle.out_ptr = ptr_len.0;
    handle.out_len = ptr_len.1;
    ptr_len.1
}

/// Render main view HTML into the handle's output buffer, returning its byte length.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_render_main(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let ptr_len = {
        let s = handle.app.render_main();
        (s.as_ptr(), s.len())
    };
    handle.out_ptr = ptr_len.0;
    handle.out_len = ptr_len.1;
    ptr_len.1
}

/// Render status JSON (tabs, counts, Cmd+F bar) into the output buffer, returning its byte length.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_render_status(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let ptr_len = {
        let s = handle.app.render_status_json();
        (s.as_ptr(), s.len())
    };
    handle.out_ptr = ptr_len.0;
    handle.out_len = ptr_len.1;
    ptr_len.1
}

/// Compute WebGPU fit bounds JSON `[cx, cy, w, h]` into the output buffer.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_fit_graph(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    let ptr_len = {
        let s = handle.app.fit_graph_json();
        (s.as_ptr(), s.len())
    };
    handle.out_ptr = ptr_len.0;
    handle.out_len = ptr_len.1;
    ptr_len.1
}

/// Emit WebGPU edge quad WGSL directly from Rust via `gpu::emit::emit_wgsl`.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_wgsl_edge(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    handle.wgsl_buf.clear();
    let _ = shaders::emit_edge_wgsl(&mut handle.wgsl_buf);
    handle.out_ptr = handle.wgsl_buf.as_ptr();
    handle.out_len = handle.wgsl_buf.len();
    handle.out_len
}

/// Emit WebGPU SDF node WGSL directly from Rust via `gpu::emit::emit_wgsl`.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_wgsl_node(h: *mut Handle) -> usize {
    if h.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *h };
    handle.wgsl_buf.clear();
    let _ = shaders::emit_node_wgsl(&mut handle.wgsl_buf);
    handle.out_ptr = handle.wgsl_buf.as_ptr();
    handle.out_len = handle.wgsl_buf.len();
    handle.out_len
}

/// Pointer to the latest rendered UTF-8 output buffer.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_out_ptr(h: *const Handle) -> *const u8 {
    if h.is_null() {
        ptr::null()
    } else {
        unsafe { (*h).out_ptr }
    }
}

/// Advance client-side relational ECS physics & pack WebGPU node/edge instance buffers.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_step(h: *mut Handle, dt: f32) {
    if h.is_null() {
        return;
    }
    let handle = unsafe { &mut *h };
    handle.app.step_graph(dt);
}

/// Pointer to packed `f32` WebGPU node instances (`8` floats per node).
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_nodes_ptr(h: *const Handle) -> *const f32 {
    if h.is_null() {
        ptr::null()
    } else {
        unsafe { (*h).app.gpu.nodes.as_ptr() }
    }
}

/// Number of `f32` elements in the packed WebGPU node buffer (`8 * node_count`).
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_nodes_len(h: *const Handle) -> usize {
    if h.is_null() {
        0
    } else {
        unsafe { (*h).app.gpu.nodes.len() }
    }
}

/// Pointer to packed `f32` WebGPU edge instances (`8` floats per edge).
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_edges_ptr(h: *const Handle) -> *const f32 {
    if h.is_null() {
        ptr::null()
    } else {
        unsafe { (*h).app.gpu.edges.as_ptr() }
    }
}

/// Number of `f32` elements in the packed WebGPU edge buffer (`8 * edge_count`).
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_edges_len(h: *const Handle) -> usize {
    if h.is_null() {
        0
    } else {
        unsafe { (*h).app.gpu.edges.len() }
    }
}

/// Pointer to packed UTF-8 JSON label coordinates for crisp 2D canvas text over WebGPU.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_labels_ptr(h: *const Handle) -> *const u8 {
    if h.is_null() {
        ptr::null()
    } else {
        unsafe { (*h).app.gpu.labels.as_ptr() }
    }
}

/// Byte length of the JSON label coordinates buffer.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_labels_len(h: *const Handle) -> usize {
    if h.is_null() {
        0
    } else {
        unsafe { (*h).app.gpu.labels.len() }
    }
}

/// Hit-test WebGPU graph nodes at world coordinate `(wx, wy)`; returns entity slot or `-1`.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_pick(h: *const Handle, wx: f32, wy: f32, pad: f32) -> i32 {
    if h.is_null() {
        return -1;
    }
    unsafe { (*h).app.pick_graph(wx, wy, pad) }
}

/// Pin/drag a WebGPU graph node in the client-side relational ECS.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_drag(
    h: *mut Handle,
    slot: u32,
    wx: f32,
    wy: f32,
    active: i32,
) {
    if h.is_null() {
        return;
    }
    unsafe { (*h).app.drag_graph(slot, wx, wy, active != 0) };
}

/// Update hovered node slot (`< 0` clears hover) for neighbor highlighting in WebGPU.
///
/// # Safety
/// `h` from `explorer_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explorer_graph_hover(h: *mut Handle, slot: i32) {
    if h.is_null() {
        return;
    }
    unsafe { (*h).app.hover_graph(slot) };
}
