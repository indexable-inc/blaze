use crate::sys;
use core::ffi::c_void;
use std::ffi::CString;

pub type Id = u64;

/// Handle to a flecs world. `Copy`: it is a pointer; the host owns the world and outlives every module generation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct World(*mut sys::ecs_world_t);

pub(crate) fn cs(s: &str) -> CString {
    if s.contains('\0') {
        return CString::new(s.replace('\0', "")).unwrap_or_default();
    }
    // no NUL inside: one allocation (the CString), not a `replace` copy first
    CString::new(s).unwrap_or_default()
}

/// Longest name `with_cstr` puts on the stack; longer ones take the heap path.
const STACK_NAME: usize = 255;

/// Run `f` with `name` as a NUL terminated C string without allocating (a name of up to [`STACK_NAME`] bytes without NUL goes through a stack buffer). The hot
/// by-name lookups of every system tick use it.
pub(crate) fn with_cstr<R>(name: &str, f: impl FnOnce(*const core::ffi::c_char) -> R) -> R {
    let b = name.as_bytes();
    if b.len() <= STACK_NAME && !b.contains(&0) {
        let mut buf = [0u8; STACK_NAME + 1];
        buf[..b.len()].copy_from_slice(b);
        return f(buf.as_ptr().cast());
    }
    let n = cs(name);
    f(n.as_ptr())
}

/// Relationship pair id `(rel, tgt)`; no function call, so no symbol needed beyond `ECS_PAIR`.
pub fn pair(rel: Id, tgt: Id) -> Id {
    // SAFETY: ECS_PAIR is an immutable flecs global resolved from the host.
    unsafe { sys::ECS_PAIR | (rel << 32) | (tgt & 0xFFFF_FFFF) }
}

impl World {
    /// # Safety
    /// `p` is a live `ecs_world_t*` that outlives every use of the handle.
    pub unsafe fn from_raw(p: *mut c_void) -> World {
        World(p.cast())
    }
    pub fn as_ptr(self) -> *mut sys::ecs_world_t {
        self.0
    }
    pub fn as_c_void(self) -> *mut c_void {
        self.0.cast()
    }

    /// Host only: create a world with every addon the linked flecs was built with imported.
    ///
    /// # Safety
    /// flecs's OS API (allocator, abort) must be set before the first world: den-ecs's `os_api` sets it.
    pub unsafe fn init() -> World {
        World(unsafe { sys::ecs_init() })
    }
    /// # Safety
    /// No module may hold handles into the world; call after every module is unloaded.
    pub unsafe fn fini(self) {
        unsafe { sys::ecs_fini(self.0) };
    }
    /// Run one tick (host, simulation thread only).
    #[cfg(feature = "pipeline")]
    pub fn progress(self, dt: f32) -> bool {
        // SAFETY: world is live; called on the simulation thread.
        unsafe { sys::ecs_progress(self.0, dt) }
    }

    pub fn lookup(self, name: &str) -> Id {
        // SAFETY: the pointer is NUL terminated for the call.
        with_cstr(name, |p| unsafe { sys::ecs_lookup(self.0, p) })
    }
    /// Find or create the named entity (zero-size tag when it has no component).
    pub fn ensure_entity(self, name: &str) -> Id {
        let n = cs(name);
        // SAFETY: zeroed desc is valid (flecs "unset"); name outlives the call (flecs copies it).
        unsafe {
            let mut d: sys::ecs_entity_desc_t = core::mem::zeroed();
            d.name = n.as_ptr();
            sys::ecs_entity_init(self.0, &d)
        }
    }
    pub fn ensure_tag(self, name: &str) -> Id {
        self.ensure_entity(name)
    }
    pub fn new_entity(self) -> Id {
        // SAFETY: world live.
        unsafe { sys::ecs_new(self.0) }
    }
    /// The current flecs scope (0 at the root).
    pub fn scope(self) -> Id {
        // SAFETY: world live.
        unsafe { sys::ecs_get_scope(self.0) }
    }
    /// A fresh anonymous entity that is a child of the current scope (so it dies with the module's `systems.<name>`), or 0 at the root scope.
    /// Unnamed queries made in `init` use it as their entity: the host's teardown then frees them and no module `deinit` has to `fini` them.
    pub fn scoped_anon(self) -> Id {
        let s = self.scope();
        if s == 0 {
            return 0;
        }
        // SAFETY: world live; `s` is the live scope entity.
        unsafe {
            let e = sys::ecs_new(self.0);
            sys::ecs_add_id(self.0, e, pair(sys::EcsChildOf, s));
            e
        }
    }
    pub fn delete(self, e: Id) {
        // SAFETY: world live; deleting a dead id is a no-op in flecs.
        unsafe { sys::ecs_delete(self.0, e) }
    }
    pub fn is_alive(self, e: Id) -> bool {
        // SAFETY: world live.
        unsafe { sys::ecs_is_alive(self.0, e) }
    }
    pub fn set_scope(self, e: Id) -> Id {
        // SAFETY: world live.
        unsafe { sys::ecs_set_scope(self.0, e) }
    }
    /// Run `f` with the flecs scope at the root, then restore the previous scope (also on unwind). Module init runs inside `systems.<name>`:
    /// `ensure_entity("modules.x")` or a dotted component name created there lands UNDER the scope (`systems.<name>.modules.x`), while `lookup`
    /// falls back to the root, so a name the host owns at the root must be created and looked up through this.
    pub fn at_root<R>(self, f: impl FnOnce() -> R) -> R {
        struct Restore(World, Id);
        impl Drop for Restore {
            fn drop(&mut self) {
                self.0.set_scope(self.1);
            }
        }
        let _restore = Restore(self, self.set_scope(0));
        f()
    }
    pub fn has_id(self, e: Id, id: Id) -> bool {
        // SAFETY: world live.
        unsafe { sys::ecs_has_id(self.0, e, id) }
    }
    pub fn add_id(self, e: Id, id: Id) {
        // SAFETY: world live.
        unsafe { sys::ecs_add_id(self.0, e, id) }
    }
    pub fn remove_id(self, e: Id, id: Id) {
        // SAFETY: world live.
        unsafe { sys::ecs_remove_id(self.0, e, id) }
    }
    pub fn count_id(self, id: Id) -> i32 {
        // SAFETY: world live.
        unsafe { sys::ecs_count_id(self.0, id) }
    }
    /// Number of alive entities in the world (flecs `ecs_get_entities().alive_count`): the host's per-reload leak check compares it before and after.
    pub fn alive_entities(self) -> i64 {
        // SAFETY: world live; read-only snapshot of the entity index header.
        unsafe { sys::ecs_get_entities(self.0).alive_count as i64 }
    }
    /// Every system and observer entity in the world (their `(EcsPoly, EcsSystem|EcsObserver)` pair), wherever it lives in the name tree.
    /// The host diffs this around `plugin_init` to catch registrations made outside the module scope (they survive `deinit`).
    #[cfg(feature = "system")]
    pub fn callables(self) -> Vec<Id> {
        let mut out = vec![];
        // SAFETY: immutable flecs globals; the query is finished before returning; `entities` is read within the iteration.
        unsafe {
            for tag in [sys::EcsSystem, sys::EcsObserver] {
                let mut d: sys::ecs_query_desc_t = core::mem::zeroed();
                d.terms[0].id = pair(sys::FLECS_IDEcsPolyID_, tag);
                let q = sys::ecs_query_init(self.0, &d);
                if q.is_null() {
                    continue;
                }
                let mut it = sys::ecs_query_iter(self.0, q);
                while sys::ecs_query_next(&mut it) {
                    for k in 0..it.count as usize {
                        out.push(*it.entities.add(k));
                    }
                }
                sys::ecs_query_fini(q);
            }
        }
        out
    }
    /// Dotted path of `e` below `root` (`root` itself excluded; `None` when `e` is not under it): the names of its ancestors up to `root`.
    pub fn path_below(self, e: Id, root: Id) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = e;
        while cur != 0 && cur != root {
            parts.push(self.name(cur)?);
            // SAFETY: world live; a root-scope entity has parent 0.
            cur = unsafe { sys::ecs_get_parent(self.0, cur) };
        }
        (cur == root && root != 0).then(|| {
            parts.reverse();
            parts.join(".")
        })
    }
    /// The direct children of `parent` (entities whose `ChildOf` is `parent`), in table order. Reads committed state.
    pub fn children(self, parent: Id) -> Vec<Id> {
        let mut out = vec![];
        // SAFETY: the iterator is driven to completion; `entities` is valid for `count` ids until the next call.
        unsafe {
            let mut it = sys::ecs_children(self.0, parent);
            while sys::ecs_children_next(&mut it) {
                for k in 0..it.count as usize {
                    out.push(*it.entities.add(k));
                }
            }
        }
        out
    }
    pub fn name(self, e: Id) -> Option<String> {
        // SAFETY: flecs returns a NUL terminated string owned by the world, or null.
        unsafe {
            let p = sys::ecs_get_name(self.0, e);
            (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }
    /// Full dotted path of `e` from the root scope (`blockphys.block.oak_door`), `None` for an unnamed entity. `name` is only the last segment.
    pub fn path(self, e: Id) -> Option<String> {
        let mut parts = vec![self.name(e)?];
        // SAFETY: world live; a root-scope entity has parent 0.
        let mut cur = unsafe { sys::ecs_get_parent(self.0, e) };
        while cur != 0 {
            parts.push(self.name(cur)?);
            cur = unsafe { sys::ecs_get_parent(self.0, cur) };
        }
        parts.reverse();
        Some(parts.join("."))
    }
    /// `e`'s full dotted path with `prefix` (written with its trailing dot: `"bubbles.tag."`) removed; `None` when `e` is unnamed or not under it.
    /// The one way a module reads a row key from an entity name: `name` is only the LAST segment, so `name(e).starts_with("a.b.")` is never true.
    pub fn path_rest(self, e: Id, prefix: &str) -> Option<String> {
        self.path(e)?.strip_prefix(prefix).map(str::to_owned)
    }
    /// The target of the first `(rel, *)` pair on `e` (relationship target), or 0.
    pub fn target(self, e: Id, rel: Id) -> Id {
        // SAFETY: world live.
        unsafe { sys::ecs_get_target(self.0, e, rel, 0) }
    }

    /// Every `(rel, *)` pair on `e` as `(target, pair id)`; `target` is the live entity or 0 when it is gone (flecs normally removes
    /// the pair with a deleted target, so 0 is the one-tick window before cleanup). Type order. Reads the entity's type, so it sees
    /// only committed state (not changes deferred inside a system).
    pub fn pairs_of(self, e: Id, rel: Id) -> Vec<(Id, Id)> {
        const PAIR_FLAG: u64 = 1 << 63;
        let mut out = Vec::new();
        // SAFETY: the type array is valid until the next structural change; we copy ids out immediately.
        unsafe {
            let ty = sys::ecs_get_type(self.0, e);
            if ty.is_null() {
                return out;
            }
            for &id in core::slice::from_raw_parts((*ty).array, (*ty).count as usize) {
                if id & PAIR_FLAG != 0 && ((id & 0x00FF_FFFF_FFFF_FFFF) >> 32) == (rel & 0xFFFF_FFFF) {
                    out.push((sys::ecs_get_alive(self.0, id & 0xFFFF_FFFF), id));
                }
            }
        }
        out
    }
}
