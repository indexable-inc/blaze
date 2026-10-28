//! The content scripts to a snapshot, for gen/build.ts under Bun: the scripts run in Flecs (compiled into this
//! module for wasm32-unknown-unknown), what the build baked (highlighted code, drawings, narration) is set on its entities, and the world comes back as the
//! flecs-snapshot DAG-CBOR document `site_load` reads. Build time only; the worker's module (`site-wasm`) has no Flecs.
//!
//! ```text
//! h = snap_new()
//! p = snap_alloc(n); write the input at p; snap_build(h, p, n); snap_free(p, n)   0 with the snapshot, or 1 with the error
//! read snap_output_len(h) bytes at snap_output_ptr(h); snap_drop(h)
//! ```
//!
//! The input is JSON: `{scripts: [{name, code}], bakes: {<entity path>: {highlighted?, baked?, audio?}}}`, the scripts in run
//! order (schema.flecs, refs.flecs, then the rest by path, then what the build generates).

use std::collections::BTreeMap;

use serde::Deserialize;
use site_render::scripts::{self, Bake, Script};

#[derive(Deserialize)]
struct Input {
    scripts: Vec<ScriptJson>,
    bakes: BTreeMap<String, BakeJson>,
}

#[derive(Deserialize)]
struct ScriptJson {
    name: String,
    code: String,
}

#[derive(Deserialize)]
struct BakeJson {
    highlighted: Option<String>,
    baked: Option<String>,
    audio: Option<String>,
}

/// The snapshot for `input` (the JSON above).
pub fn build(input: &[u8]) -> Result<Vec<u8>, String> {
    let i: Input = serde_json::from_slice(input).map_err(|e| format!("input: {e}"))?;
    let scripts: Vec<Script> =
        i.scripts.into_iter().map(|s| Script { name: s.name, code: s.code }).collect();
    let bakes = i
        .bakes
        .into_iter()
        .map(|(k, b)| (k, Bake { highlighted: b.highlighted, baked: b.baked, audio: b.audio }))
        .collect();
    scripts::snapshot(&scripts, &bakes)
}

pub struct Handle {
    out: Vec<u8>,
}

#[unsafe(no_mangle)]
pub extern "C" fn snap_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// # Safety
/// `ptr` and `len` from one `snap_alloc`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snap_free(ptr: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

#[unsafe(no_mangle)]
pub extern "C" fn snap_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle { out: Vec::new() }))
}

/// # Safety
/// `h` from `snap_new`, dropped once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snap_drop(h: *mut Handle) {
    drop(unsafe { Box::from_raw(h) });
}

/// # Safety
/// `h` from `snap_new`; `ptr..ptr+len` readable (or `len` zero).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snap_build(h: *mut Handle, ptr: *const u8, len: usize) -> i32 {
    let h = unsafe { &mut *h };
    let input = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(ptr, len) } };
    match build(input) {
        Ok(out) => {
            h.out = out;
            0
        }
        Err(e) => {
            h.out = e.into_bytes();
            1
        }
    }
}

/// # Safety
/// `h` from `snap_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snap_output_ptr(h: *const Handle) -> *const u8 {
    unsafe { (*h).out.as_ptr() }
}

/// # Safety
/// `h` from `snap_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn snap_output_len(h: *const Handle) -> usize {
    unsafe { (*h).out.len() }
}
