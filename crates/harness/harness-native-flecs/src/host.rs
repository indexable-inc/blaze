//! Which image may register components, and how an id is resolved.
//!
//! The image that creates the world ([`World::init_host`](crate::World::init_host)) is the host: it registers every component, and
//! a component it uses before registering it is registered then (flecs-rust's behaviour). Each image has its own copy of this
//! crate's statics, so in a module image the flag stays off: a component the host has not registered, or one registered with
//! another layout, is a fault there (logged, the image quarantined) and resolves to 0, which every accessor treats as absent.
use crate::ComponentId;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use hyperion_ecs::component::Error;
use hyperion_ecs::{Id, sys};
use hyperion_ecs_core::identity::{ApiK, component_id};

static HOST: AtomicBool = AtomicBool::new(false);

pub(crate) fn mark_host() {
    HOST.store(true, Ordering::Relaxed);
}

/// True in the image that made the world.
#[must_use]
pub fn is_host() -> bool {
    HOST.load(Ordering::Relaxed)
}

pub(crate) fn resolve<T: ComponentId>(w: hyperion_ecs::World) -> Id {
    match component_id::<T, ApiK>(w) {
        Ok(id) => id,
        Err(Error::NotAComponent(_)) if is_host() => register::<T>(w),
        Err(e) if is_host() => panic!("{e}"),
        Err(e) => {
            hyperion_ecs::hooks::fault(T::NAME, &e.to_string());
            0
        }
    }
}

/// Register `T` (host only), its lifecycle hooks in this image.
pub(crate) fn register<T: ComponentId>(w: hyperion_ecs::World) -> Id {
    assert!(is_host(), "component {} registered outside the host image", T::NAME);
    const {
        assert!(size_of::<T>() <= i32::MAX as usize, "a component is larger than flecs sizes allow");
    }
    // SAFETY: a zeroed hooks struct is flecs's "unset"; each hook matches the flecs signature and touches `count` values of `T`.
    let mut hooks: sys::ecs_type_hooks_t = unsafe { core::mem::zeroed() };
    hooks.ctor = Some(ctor::<T>);
    hooks.dtor = Some(dtor::<T>);
    hooks.copy = Some(copy::<T>);
    hooks.move_ = Some(move_::<T>);
    // The world is in the root scope here: names are root-level, as every image looks them up.
    let at_root = w.set_scope(0);
    let fresh = component_id::<T, ApiK>(w).is_err();
    let id = hyperion_ecs::component::register_component_with_hooks::<T>(w, T::NAME, &hooks);
    w.set_scope(at_root);
    let id = id.unwrap_or_else(|e| panic!("{e}"));
    // Every component is the host's until its registration says otherwise (`ComponentView::writable`): a component nobody
    // thought about is one a cell cannot write.
    if fresh {
        // SAFETY: live world; `id` is the component just registered.
        unsafe { sys::ecs_add_id(w.as_ptr(), id, w.ensure_entity(crate::HOST_OWNED)) };
    }
    id
}

/// Re-raise, in the host, a panic that a system's trampoline caught (the host image does not quarantine itself).
pub(crate) fn rethrow() {
    let mut buf = [0u8; 1024];
    let n = hyperion_panic::take(&mut buf);
    if n != 0 {
        panic!("{}", String::from_utf8_lossy(&buf[..n.min(buf.len())]));
    }
}

unsafe extern "C" fn ctor<T: ComponentId>(ptr: *mut c_void, count: i32, _ti: *const sys::ecs_type_info_t) {
    let p = ptr.cast::<T>();
    for i in 0..count as usize {
        // SAFETY: flecs hands `count` uninitialised, aligned slots.
        unsafe { p.add(i).write(T::default()) };
    }
}

unsafe extern "C" fn dtor<T: ComponentId>(ptr: *mut c_void, count: i32, _ti: *const sys::ecs_type_info_t) {
    // SAFETY: flecs hands `count` initialised values it will not use again.
    unsafe { core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(ptr.cast::<T>(), count as usize)) };
}

unsafe extern "C" fn copy<T: ComponentId>(dst: *mut c_void, src: *const c_void, count: i32, _ti: *const sys::ecs_type_info_t) {
    let (d, s) = (dst.cast::<T>(), src.cast::<T>());
    for i in 0..count as usize {
        // SAFETY: both ranges hold initialised values; flecs never overlaps them.
        unsafe { (*d.add(i)).clone_from(&*s.add(i)) };
    }
}

unsafe extern "C" fn move_<T: ComponentId>(dst: *mut c_void, src: *mut c_void, count: i32, _ti: *const sys::ecs_type_info_t) {
    let (d, s) = (dst.cast::<T>(), src.cast::<T>());
    for i in 0..count as usize {
        // SAFETY: both ranges hold initialised values (move leaves `src` valid but empty, as flecs expects).
        unsafe { *d.add(i) = core::mem::take(&mut *s.add(i)) };
    }
}
