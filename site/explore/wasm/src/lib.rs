//! `blaze-explore-wasm` — 99% Pure-Rust WebGPU Observatory powered by a Flecs 4.2-inspired Relational ECS.
//!
//! Directly shares `crates/blaze-launch/src/telemetry.rs` (exact 64-byte `Event` struct) with
//! zero schema duplication and zero third-party dependencies.
//!
//! Exports a minimal C-ABI surface to the ~240-line TypeScript WebGPU/WebSocket host shim:
//!   - `init(advance_ratio, webgpu_active)`
//!   - `io_buf_ptr()` / `io_buf_cap()` (zero-copy input buffer for binary WS frames & JSON payloads)
//!   - `ingest_binary(byte_len, now_ms)`
//!   - `ingest_json(byte_len)`
//!   - `ingest_topology(byte_len)`
//!   - `ingest_source(byte_len)`
//!   - `set_connection(connected, detail_len)`
//!   - `on_pointer_move(x, y)` / `on_pointer_down(x, y, shift)` / `on_pointer_up(x, y)`
//!   - `on_dblclick(x, y)` / `on_wheel(x, y, dx, dy, ctrl_or_meta)` / `on_key_down(code)`
//!   - `render_frame(now_ms, css_w, css_h)`
//!   - `rects_ptr()` / `rects_count()` / `glyphs_ptr()` / `glyphs_count()`
//!   - `take_commands_len()` / `cmd_buf_ptr()` (drains host side-effects: `source:`, `toggle:`, `open:`, `cursor:`)

extern crate self as blaze_own;

#[doc(hidden)]
pub mod planted {
    pub trait Plants {}
    pub trait Planted<T> {
        fn has(&self, _: T) -> bool {
            false
        }
    }
    pub struct NoPlant;
    impl<T> Planted<T> for NoPlant {}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Slot(pub u32);

#[path = "../../../../crates/blaze-launch/src/telemetry.rs"]
pub mod telemetry;

pub mod dock;
pub mod ecs;
pub mod gpu;
pub mod model;
pub mod rust_ide;
pub mod ui;

use std::cell::UnsafeCell;
use ui::UiApp;

const IO_BUF_CAP: usize = 1024 * 1024;
const CMD_BUF_CAP: usize = 32 * 1024;

struct State {
    app: Option<UiApp>,
    io_buf: [u8; IO_BUF_CAP],
    cmd_buf: [u8; CMD_BUF_CAP],
}

struct SyncCell(UnsafeCell<State>);
unsafe impl Sync for SyncCell {}

static STATE: SyncCell = SyncCell(UnsafeCell::new(State {
    app: None,
    io_buf: [0; IO_BUF_CAP],
    cmd_buf: [0; CMD_BUF_CAP],
}));

#[inline]
unsafe fn state_mut() -> &'static mut State {
    unsafe { &mut *STATE.0.get() }
}

#[inline]
unsafe fn app_mut() -> &'static mut UiApp {
    let st = unsafe { state_mut() };
    st.app.get_or_insert_with(UiApp::new)
}

#[unsafe(no_mangle)]
pub extern "C" fn init(advance_ratio: f32, webgpu_active: u32) {
    let app = unsafe { app_mut() };
    if advance_ratio > 0.25 && advance_ratio < 1.2 {
        app.draw.advance_ratio = advance_ratio;
    }
    app.webgpu_active = webgpu_active != 0;
}

#[unsafe(no_mangle)]
pub extern "C" fn io_buf_ptr() -> *mut u8 {
    unsafe { state_mut().io_buf.as_mut_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn io_buf_cap() -> u32 {
    IO_BUF_CAP as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_binary(byte_len: u32, now_ms: f64) -> u32 {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    let raw: &[u8] = &st.io_buf[..len];
    let app = st.app.get_or_insert_with(UiApp::new);
    if app.paused {
        return 0;
    }
    app.world.ingest_binary(raw, now_ms)
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_json(byte_len: u32) {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    if let Ok(s) = core::str::from_utf8(&st.io_buf[..len]) {
        let app = st.app.get_or_insert_with(UiApp::new);
        app.world.ingest_json(s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_topology(byte_len: u32) {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    if let Ok(s) = core::str::from_utf8(&st.io_buf[..len]) {
        let app = st.app.get_or_insert_with(UiApp::new);
        app.world.ingest_topology_json(s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_source(byte_len: u32) {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    if let Ok(s) = core::str::from_utf8(&st.io_buf[..len]) {
        let app = st.app.get_or_insert_with(UiApp::new);
        app.world.ingest_source_json(s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_resources(byte_len: u32) {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    if let Ok(s) = core::str::from_utf8(&st.io_buf[..len]) {
        let app = st.app.get_or_insert_with(UiApp::new);
        app.world.ingest_resources_json(s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ingest_def(byte_len: u32) {
    let st = unsafe { state_mut() };
    let len = (byte_len as usize).min(IO_BUF_CAP);
    if let Ok(s) = core::str::from_utf8(&st.io_buf[..len]) {
        let app = st.app.get_or_insert_with(UiApp::new);
        app.world.ingest_def_json(s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn active_pid() -> u32 {
    unsafe { app_mut() }.world.pid
}

#[unsafe(no_mangle)]
pub extern "C" fn set_connection(connected: u32, detail_len: u32) {
    let st = unsafe { state_mut() };
    let len = (detail_len as usize).min(IO_BUF_CAP);
    let detail = core::str::from_utf8(&st.io_buf[..len]).unwrap_or("").to_string();
    let app = st.app.get_or_insert_with(UiApp::new);
    app.world.connected = connected != 0;
    app.world.conn_detail = detail;
}

#[unsafe(no_mangle)]
pub extern "C" fn on_pointer_move(x: f32, y: f32) {
    unsafe { app_mut() }.on_pointer_move(x, y);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_pointer_down(x: f32, y: f32, shift: u32) {
    unsafe { app_mut() }.on_pointer_down(x, y, shift != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_pointer_up(x: f32, y: f32) {
    unsafe { app_mut() }.on_pointer_up(x, y);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_dblclick(x: f32, y: f32) {
    unsafe { app_mut() }.on_dblclick(x, y);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_wheel(x: f32, y: f32, dx: f32, dy: f32, ctrl_or_meta: u32) {
    unsafe { app_mut() }.on_wheel(x, y, dx, dy, ctrl_or_meta != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn on_key_down(code: u32) {
    unsafe { app_mut() }.on_key_down(code);
}

#[unsafe(no_mangle)]
pub extern "C" fn render_frame(now_ms: f64, css_w: f32, css_h: f32) {
    unsafe { app_mut() }.render_frame(now_ms, css_w, css_h);
}

#[unsafe(no_mangle)]
pub extern "C" fn rects_ptr() -> *const gpu::RectInst {
    unsafe { app_mut() }.draw.rects.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn rects_count() -> u32 {
    unsafe { app_mut() }.draw.rects.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn glyphs_ptr() -> *const gpu::GlyphInst {
    unsafe { app_mut() }.draw.glyphs.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn glyphs_count() -> u32 {
    unsafe { app_mut() }.draw.glyphs.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn ov_rects_ptr() -> *const gpu::RectInst {
    unsafe { app_mut() }.draw.ov_rects.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn ov_rects_count() -> u32 {
    unsafe { app_mut() }.draw.ov_rects.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn ov_glyphs_ptr() -> *const gpu::GlyphInst {
    unsafe { app_mut() }.draw.ov_glyphs.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn ov_glyphs_count() -> u32 {
    unsafe { app_mut() }.draw.ov_glyphs.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn cmd_buf_ptr() -> *const u8 {
    unsafe { state_mut().cmd_buf.as_ptr() }
}

/// Drains pending host commands (newline-delimited: `cursor:...`, `source:...`, `toggle:...`, `open:...`).
#[unsafe(no_mangle)]
pub extern "C" fn take_commands_len() -> u32 {
    let st = unsafe { state_mut() };
    let app = st.app.get_or_insert_with(UiApp::new);
    let mut out = String::with_capacity(256);
    out.push_str("cursor:");
    out.push_str(app.desired_cursor);
    out.push('\n');
    for cmd in app.commands.drain(..) {
        out.push_str(&cmd);
        out.push('\n');
    }
    let bytes = out.as_bytes();
    let len = bytes.len().min(CMD_BUF_CAP);
    st.cmd_buf[..len].copy_from_slice(&bytes[..len]);
    len as u32
}
