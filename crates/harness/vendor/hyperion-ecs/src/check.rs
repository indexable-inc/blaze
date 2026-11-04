//! Checked component access (docs/module-safety.md section 3): `Iter::field::<T>`, `World::set/get/get_mut::<T>` compare `size_of::<T>()` with the
//! size the world registered for the component BEFORE touching memory. A mismatch used to be an `ecs_assert` abort inside flecs (or, in a release
//! flecs, a silent out-of-bounds read); now it is reported as a module fault (`component 'X' layout mismatch (...)`), the operation is skipped and
//! the image is quarantined, so the host can roll back to the previous build. Once per id and image: a verified id is cached (direct-mapped, no
//! locking, a miss costs one `ecs_get_type_info`); component ids are never reused for another layout (a layout change deletes and recreates the
//! entity with a new generation).
use crate::sys;
use crate::world::{Id, World};
use core::ffi::CStr;
use core::sync::atomic::{AtomicU64, Ordering};

const WAYS: usize = 128;
static OK: [AtomicU64; WAYS] = [const { AtomicU64::new(0) }; WAYS];
static REPORTED: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

fn slot(id: Id) -> usize {
    ((id ^ (id >> 29) ^ (id >> 13)) as usize) % WAYS
}

/// Full dotted path of the component an id stands for (the first element of a pair).
pub fn component_path(w: World, id: Id) -> String {
    name_of(w.as_ptr(), id)
}

fn name_of(w: *mut sys::ecs_world_t, id: Id) -> String {
    // SAFETY: live world; the path string is freed with the OS-API free.
    unsafe {
        let base = if id & sys::ECS_PAIR != 0 { sys::ecs_get_alive(w, (id >> 32) & 0xFFFF_FFFF) } else { id };
        let p = sys::ecs_get_path_w_sep(w, 0, base, c".".as_ptr(), core::ptr::null());
        if p.is_null() {
            return format!("#{id:x}");
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        if let Some(free) = sys::ecs_os_api.free_ {
            free(p.cast());
        }
        s
    }
}

fn world_hash(w: World, id: Id) -> String {
    let base = if id & unsafe { sys::ECS_PAIR } != 0 { id >> 32 & 0xFFFF_FFFF } else { id };
    let name = name_of(w.as_ptr(), base);
    match crate::component::world_layout_hash(w, &name) {
        Some(h) => format!("{h:x}"),
        None => "none".into(),
    }
}

#[cold]
fn report(w: World, id: Id, world_size: usize, built: usize, what: &str) {
    let first = REPORTED.iter().any(|r| r.load(Ordering::Relaxed) == id);
    if first {
        return;
    }
    if let Some(r) = REPORTED.iter().find(|r| r.load(Ordering::Relaxed) == 0) {
        r.store(id, Ordering::Relaxed);
    }
    let name = name_of(w.as_ptr(), id);
    let msg = format!(
        "component '{name}' layout mismatch (world size {world_size} hash {}, built against size {built}) in {what}; the module was built against another layout of '{name}': rebuild it (and the module that provides '{name}')",
        world_hash(w, id)
    );
    crate::hooks::fault(what, &msg);
}

fn key(id: Id, size: usize) -> u64 {
    // One word per way, so a reader never pairs the id of one entry with the size of another.
    id ^ (size as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
}

/// True when `size_of::<T>()` (`built`) equals the size of component `id` in the world. A tag (no type info) matches size 0.
pub fn size_ok(w: World, id: Id, built: usize, what: &str) -> bool {
    let k = key(id, built);
    let s = slot(id);
    if OK[s].load(Ordering::Relaxed) == k {
        return true;
    }
    // SAFETY: live world.
    let ti = unsafe { sys::ecs_get_type_info(w.as_ptr(), id) };
    let have = if ti.is_null() { 0 } else { unsafe { (*ti).size as usize } };
    if have == built {
        OK[s].store(k, Ordering::Relaxed);
        return true;
    }
    report(w, id, have, built, what);
    false
}

/// Read check for `World::get`: a prefix view (`built` <= the world size) copies the leading bytes of a larger component, which is a deliberate
/// pattern (a module that needs only the first fields of a core table); only a type LARGER than the world's component is a mismatch.
pub fn size_ok_read(w: World, id: Id, built: usize, what: &str) -> bool {
    let k = key(id, built);
    let s = slot(id);
    if OK[s].load(Ordering::Relaxed) == k {
        return true;
    }
    // SAFETY: live world.
    let ti = unsafe { sys::ecs_get_type_info(w.as_ptr(), id) };
    let have = if ti.is_null() { 0 } else { unsafe { (*ti).size as usize } };
    if built <= have {
        OK[s].store(k, Ordering::Relaxed);
        return true;
    }
    report(w, id, have, built, what);
    false
}

/// `Iter::field` check: the iterator knows the size of column `index` (`ecs_field_size`), no lookup needed.
pub fn field_ok(it: &sys::ecs_iter_t, index: i8, built: usize, what: &str) -> bool {
    // SAFETY: valid iterator; `ecs_field_size` reads the iterator's size array.
    let have = unsafe { sys::ecs_field_size(it, index) };
    if have == built || have == 0 {
        // size 0: a tag or an unset optional term; flecs returns a null column (an empty slice), no assert
        return true;
    }
    // A ZST column type on a tag term is fine (`field::<()>`); anything else is a layout mismatch.
    // SAFETY: valid iterator.
    let id = unsafe { sys::ecs_field_id(it, index) };
    // SAFETY: the real world of the iterator.
    let w = unsafe { World::from_raw(it.world.cast()) };
    report(w, id, have, built, what);
    false
}
