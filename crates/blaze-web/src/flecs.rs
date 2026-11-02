//! The Flecs C calls of the build page's world, behind a safe API: the crate's only unsafe
//! code (docs/v2/plan/design/unsafe.md). Flecs is the only way to the world the page's REST
//! reads, so this is FFI, not a fast path. What makes each call sound, and where it is upheld:
//!
//! - **One thread.** [`Flecs`] holds raw pointers, so it is neither `Send` nor `Sync`, and
//!   every call goes through it, `&mut` when it writes.
//! - **Live entities.** Every entity handed in is checked with `ecs_is_alive` (an O(1) lookup
//!   in Flecs's entity index) before Flecs is asked anything of it. A dead one would be a bug
//!   in the caller's bookkeeping (`world.rs`), and it stops the thread rather than reach Flecs,
//!   whose release build does not check.
//! - **Layouts.** A component is only set or read as the Rust type Flecs lays it out as. The
//!   component ids are this module's alone, each used by the one method of its type, and
//!   [`Flecs::new`] checks each type's size and alignment against Flecs's `EcsComponent`.
//! - **Copies.** Flecs copies what it is set: its `string` and `vector` types (in [`KINDS`])
//!   have copy hooks, and `ecs_set_name` copies the name. So the buffers they come from may
//!   change once the call returns.
//! - **Reads.** What [`Flecs::function`] returns points into Flecs's tables, which stay put
//!   until the world is next written. A write takes `&mut self`, and the read borrows `&self`.
//!
//! The safe reference for what goes through here is in the tests: the world's calls, read back
//! through its snapshot, are checked against a model with no Flecs
//! (`calls_waiting_in_chains_are_those_a_scan_found`), and its entities through Flecs's REST.

#![allow(unsafe_code)]

use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr,
};

use flecs_c as ecs;

pub(crate) type Entity = ecs::ecs_entity_t;

/// The kinds the world's entities have, as Flecs script.
const KINDS: &str = "
struct Workspace(root: string, build: u64)
struct Function(key: u64, name: string, how: u8, ns: u64, build: u64)
vector Callees(entity)
Crate {}
Module {}
Calls { Relationship }
";

/// A workspace, as Flecs has it (its layout is [`KINDS`]'s).
#[repr(C)]
struct Workspace {
    root: *const c_char,
    build: u64,
}

/// A function, as Flecs has it (its layout is [`KINDS`]'s).
#[repr(C)]
struct Function {
    key: u64,
    name: *const c_char,
    how: u8,
    ns: u64,
    build: u64,
}

/// Flecs's `EcsParent`.
#[repr(C)]
struct Parent {
    value: Entity,
}

/// Flecs's `EcsComponent` (`flecs.h`: two `ecs_size_t`, which is `int32_t`).
#[repr(C)]
struct Component {
    size: i32,
    alignment: i32,
}

/// What an entity named under another is.
#[derive(Clone, Copy)]
pub(crate) enum Tag {
    Crate,
    Module,
}

/// What Flecs's REST answered.
pub struct Answer {
    pub code: u16,
    pub kind: String,
    pub body: Vec<u8>,
}

/// A function's fields, as Flecs has them, until the world is next written.
pub(crate) struct FunctionRead<'a> {
    pub(crate) key: u64,
    pub(crate) how: u8,
    pub(crate) ns: u64,
    pub(crate) build: u64,
    pub(crate) name: &'a [u8],
}

/// A Flecs world of the page's kinds, and its REST server.
pub(crate) struct Flecs {
    world: *mut ecs::ecs_world_t,
    rest: *mut ecs::ecs_http_server_t,
    workspace: Entity,
    function: Entity,
    callees: Entity,
    parent: Entity,
    krate: Entity,
    module: Entity,
    calls: Entity,
    /// A string as Flecs is handed it, NUL-terminated.
    text: Vec<u8>,
}

impl Flecs {
    pub(crate) fn new() -> Result<Flecs, String> {
        // SAFETY: `ecs_init` has no precondition. A null world is refused next, and every
        // other world is freed by `Drop` from here on.
        let world = unsafe { ecs::ecs_init() };
        if world.is_null() {
            return Err("Flecs made no world".into());
        }
        let mut flecs = Flecs {
            world,
            rest: ptr::null_mut(),
            workspace: 0,
            function: 0,
            callees: 0,
            parent: 0,
            krate: 0,
            module: 0,
            calls: 0,
            // alloc: none until it fills
            text: Vec::new(),
        };
        // alloc: once per world, the kinds' script; the error's is an error path
        let code = CString::new(KINDS).map_err(|e| e.to_string())?;
        // SAFETY: a live world; both strings are NUL-terminated and outlive the call.
        let ran = unsafe {
            ecs::ecs_script_run(world, c"blaze-web".as_ptr(), code.as_ptr(), ptr::null_mut())
        };
        if ran != 0 {
            return Err("Flecs did not take the build page's kinds".into());
        }
        // SAFETY: a live world with no REST server yet; `Drop` frees the one made.
        flecs.rest = unsafe { ecs::glue_rest_new(world) };
        if flecs.rest.is_null() {
            return Err("Flecs made no REST server".into());
        }
        let workspace = (size_of::<Workspace>(), align_of::<Workspace>());
        flecs.workspace = flecs.component(c"Workspace", workspace.0, workspace.1)?;
        let function = (size_of::<Function>(), align_of::<Function>());
        flecs.function = flecs.component(c"Function", function.0, function.1)?;
        let vector = (size_of::<ecs::ecs_vec_t>(), align_of::<ecs::ecs_vec_t>());
        flecs.callees = flecs.component(c"Callees", vector.0, vector.1)?;
        // SAFETY: an extern static that Flecs's bootstrap (`ecs_init`, above) writes once and
        // nothing writes after.
        let parent = unsafe { ecs::FLECS_IDEcsParentID_ };
        flecs.laid_out(parent, "Parent", size_of::<Parent>(), align_of::<Parent>())?;
        flecs.parent = parent;
        flecs.krate = flecs.lookup(c"Crate")?;
        flecs.module = flecs.lookup(c"Module")?;
        flecs.calls = flecs.lookup(c"Calls")?;
        Ok(flecs)
    }

    /// A new entity, with nothing on it.
    pub(crate) fn entity(&mut self) -> Entity {
        // SAFETY: a live world (`new`), written on its one thread (`&mut self`).
        unsafe { ecs::ecs_new(self.world) }
    }

    /// Deletes `entity` and everything in it.
    pub(crate) fn delete(&mut self, entity: Entity) {
        self.alive(entity);
        // SAFETY: a live world and entity (checked above).
        unsafe { ecs::ecs_delete(self.world, entity) };
    }

    pub(crate) fn set_workspace(&mut self, entity: Entity, root: &[u8], build: u64) {
        let root = c_text(&mut self.text, root);
        self.set(entity, self.workspace, &Workspace { root, build });
    }

    pub(crate) fn set_function(
        &mut self,
        entity: Entity,
        key: u64,
        name: &[u8],
        how: u8,
        ns: u64,
        build: u64,
    ) {
        let name = c_text(&mut self.text, name);
        self.set(entity, self.function, &Function { key, name, how, ns, build });
    }

    /// Puts `entity` in `parent` with Flecs's `Parent`, a `ChildOf` that does not fragment.
    pub(crate) fn set_parent(&mut self, entity: Entity, parent: Entity) {
        self.alive(parent);
        self.set(entity, self.parent, &Parent { value: parent });
    }

    /// Sets `callees` as the `Callees` of `entity`.
    pub(crate) fn set_callees(&mut self, entity: Entity, callees: &[Entity]) {
        // Flecs only reads the array, to copy it.
        let vector = ecs::ecs_vec_t {
            array: callees.as_ptr().cast_mut().cast::<c_void>(),
            count: callees.len() as i32,
            size: callees.len() as i32,
        };
        self.set(entity, self.callees, &vector);
    }

    /// A new entity named `name` in `parent`, tagged `tag`.
    pub(crate) fn named(&mut self, parent: Entity, name: &[u8], tag: Tag) -> Entity {
        let entity = self.entity();
        self.set_parent(entity, parent);
        let name = c_text(&mut self.text, name);
        // SAFETY: a live world and entity (made above), and a NUL-terminated name that
        // Flecs copies and that is not changed during the call.
        unsafe { ecs::ecs_set_name(self.world, entity, name) };
        let tag = match tag {
            Tag::Crate => self.krate,
            Tag::Module => self.module,
        };
        // SAFETY: a live world and entity, and a tag `new` found in it.
        unsafe { ecs::ecs_add_id(self.world, entity, tag) };
        entity
    }

    /// Adds `(Calls, to)` to `from`.
    pub(crate) fn add_calls(&mut self, from: Entity, to: Entity) {
        self.alive(from);
        self.alive(to);
        let pair = ecs::ECS_PAIR | ((self.calls & 0xFFFF_FFFF) << 32) | (to & 0xFFFF_FFFF);
        // SAFETY: a live world, two live entities (checked above), and `Calls`, a relationship
        // `new` found, paired with one of them.
        unsafe { ecs::ecs_add_id(self.world, from, pair) };
    }

    /// The `Function` of `entity`, if it has one.
    pub(crate) fn function(&self, entity: Entity) -> Option<FunctionRead<'_>> {
        self.alive(entity);
        // SAFETY: a live world and entity (checked above). Flecs answers null for an entity
        // without the component.
        let at = unsafe { ecs::ecs_get_id(self.world, entity, self.function) }.cast::<Function>();
        if at.is_null() || !at.is_aligned() {
            return None;
        }
        // SAFETY: non-null and aligned (checked above), laid out as `Function` (checked in
        // `new`), and left alone by Flecs until the world is next written, which takes
        // `&mut self` while this borrows `&self`.
        let function = unsafe { &*at };
        if function.name.is_null() {
            return None;
        }
        // SAFETY: Flecs's copy of the name `set_function` gave it, NUL-terminated and freed
        // only when the world is next written (as above).
        let name = unsafe { CStr::from_ptr(function.name) }.to_bytes();
        let (key, how, ns, build) = (function.key, function.how, function.ns, function.build);
        Some(FunctionRead { key, how, ns, build, name })
    }

    /// What Flecs's REST answers `method` of `path` (with its query), read only.
    pub(crate) fn rest(&mut self, method: &str, path: &str) -> Result<Answer, String> {
        let method = CString::new(method).map_err(|_| "a NUL in the method")?;
        let path = CString::new(path).map_err(|_| "a NUL in the path")?;
        let (mut body, mut len, mut kind) = (ptr::null_mut(), 0i32, ptr::null());
        // SAFETY: the world's live REST server (`new`), on the world's thread (`&mut self`),
        // NUL-terminated strings that outlive the call, and three places for what it writes.
        let code = unsafe {
            ecs::glue_rest_request(
                self.rest,
                method.as_ptr(),
                path.as_ptr(),
                ptr::null(),
                &mut body,
                &mut len,
                &mut kind,
            )
        };
        let bytes = if body.is_null() || len <= 0 {
            // alloc: none, an empty body
            Vec::new()
        } else {
            // SAFETY: the reply body Flecs wrote, `len` bytes, freed only below, after this
            // copy.
            unsafe { std::slice::from_raw_parts(body.cast::<u8>().cast_const(), len as usize) }
                // alloc: the edge, once per REST request
                .to_vec()
        };
        // SAFETY: the body `glue_rest_request` allocated, or null, which it takes. It is freed
        // once and not read after.
        unsafe { ecs::glue_free(body.cast::<c_void>()) };
        let kind = if kind.is_null() {
            // alloc: the edge, once per REST request
            "application/json".to_owned()
        } else {
            // SAFETY: a static NUL-terminated content type (`glue_rest_request`'s contract in
            // csrc/glue.c).
            unsafe { CStr::from_ptr(kind) }.to_string_lossy().into_owned()
        };
        // alloc: an error path
        let code = u16::try_from(code).map_err(|_| format!("Flecs answered {code}"))?;
        Ok(Answer { code, kind, body: bytes })
    }

    /// Sets the component `id` of `entity` to `value`. Called only by the setter of `id`'s
    /// type, so `T` is the type `new` checked `id` to be laid out as.
    fn set<T>(&mut self, entity: Entity, id: Entity, value: &T) {
        self.alive(entity);
        // SAFETY: a live world and entity (checked above); `id` is laid out as `T` (above);
        // Flecs copies `size_of::<T>()` bytes from `value`, and the strings and vectors they
        // point to, before it returns.
        unsafe {
            ecs::ecs_set_id(
                self.world,
                entity,
                id,
                size_of::<T>(),
                ptr::from_ref(value).cast::<c_void>(),
            )
        };
    }

    /// Stops the thread unless `entity` is one of the world's, alive.
    fn alive(&self, entity: Entity) {
        // SAFETY: a live world; any id may be asked of (0 is never alive, and is not asked).
        let alive = entity != 0 && unsafe { ecs::ecs_is_alive(self.world, entity) };
        assert!(alive, "build page: {entity:#x} is no live entity of the world");
    }

    fn lookup(&self, name: &CStr) -> Result<Entity, String> {
        // SAFETY: a live world and a NUL-terminated name.
        let entity = unsafe { ecs::ecs_lookup(self.world, name.as_ptr()) };
        if entity == 0 {
            let name = name.to_string_lossy();
            // alloc: an error path
            return Err(format!("Flecs has no {name} of the build page's kinds"));
        }
        Ok(entity)
    }

    /// The component `name`, once checked to be laid out in `size` bytes aligned to `align`.
    fn component(&self, name: &CStr, size: usize, align: usize) -> Result<Entity, String> {
        let entity = self.lookup(name)?;
        self.laid_out(entity, &name.to_string_lossy(), size, align)?;
        Ok(entity)
    }

    fn laid_out(&self, id: Entity, name: &str, size: usize, align: usize) -> Result<(), String> {
        // SAFETY: an extern static that Flecs's bootstrap writes once and nothing writes after.
        let of_component = unsafe { ecs::FLECS_IDEcsComponentID_ };
        // SAFETY: a live world; Flecs answers null for an id that is no component.
        let at = unsafe { ecs::ecs_get_id(self.world, id, of_component) }.cast::<Component>();
        if at.is_null() || !at.is_aligned() {
            // alloc: an error path
            return Err(format!("Flecs has no component {name}"));
        }
        // SAFETY: non-null and aligned (checked above), Flecs's `EcsComponent`, which is laid
        // out as `Component`, read at once, before the world is written.
        let Component { size: has, alignment } = unsafe { at.read() };
        if has as usize != size || alignment as usize != align {
            // alloc: an error path
            return Err(format!(
                "Flecs lays out {name} in {has} bytes aligned to {alignment}, \
                 not {size} aligned to {align}"
            ));
        }
        Ok(())
    }
}

impl Drop for Flecs {
    fn drop(&mut self) {
        if !self.rest.is_null() {
            // SAFETY: the REST server `new` made, freed once, before its world.
            unsafe { ecs::glue_rest_free(self.rest) };
        }
        // SAFETY: the world `new` made, freed once; nothing uses it after.
        unsafe { ecs::ecs_fini(self.world) };
    }
}

/// `text` without its NULs and with one at its end, in `into`, for Flecs to copy: valid until
/// `into` is next changed.
fn c_text(into: &mut Vec<u8>, text: &[u8]) -> *const c_char {
    into.clear();
    if text.contains(&0) {
        into.extend(text.iter().copied().filter(|&it| it != 0));
    } else {
        into.extend_from_slice(text);
    }
    into.push(0);
    into.as_ptr().cast()
}
