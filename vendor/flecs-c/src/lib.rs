//! Flecs's C (built by build.rs) and the few declarations Rust calls it through. With the `host` feature this crate
//! compiles only csrc/glue.c and links the application's Flecs through flecs_ecs_sys (SPEC.md, "Host worlds").
//!
//! On wasm32-unknown-unknown there is no C library, so this crate also provides the parts of one that are not in
//! wasm-libc/: malloc, calloc, realloc and free on Rust's allocator (with a 16-byte header holding the size, adapted
//! from den's vendor/hyperion-flecs-c), abort and exit as traps, and strtod/strtof/atof on Rust's float parser, which
//! rounds exactly as a native libc does. Natively the C library provides all of them.
//!
//! The structs below mirror Flecs's own (`flecs.h`); `tests` compares their sizes, alignments and the member offsets
//! Rust reads with what the C compiler saw, on every target.
#![no_std]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

#[cfg(target_arch = "wasm32")]
extern crate alloc;

use core::ffi::{c_char, c_int, c_void};

pub type ecs_entity_t = u64;
pub type ecs_id_t = u64;

#[repr(C)]
pub struct ecs_world_t {
    _private: [u8; 0],
}

/// Opaque: a value plus a meta cursor over it (csrc/glue.c).
#[repr(C)]
pub struct glue_writer_t {
    _private: [u8; 0],
}

/// Opaque: only ever used through a pointer from `glue_writer_cursor`.
#[repr(C)]
pub struct ecs_meta_cursor_t {
    _private: [u8; 0],
}

/// Opaque: a Flecs table, compared by address only.
#[repr(C)]
pub struct ecs_table_t {
    _private: [u8; 0],
}

/// The callbacks `glue_opaque_serialize` calls: one value of `type` at `ptr`, and a member name before a struct-shaped
/// opaque's value. Return 0 to go on, non-zero to stop with an error.
pub type glue_value_t =
    unsafe extern "C" fn(ctx: *mut c_void, type_: ecs_entity_t, ptr: *const c_void) -> c_int;
pub type glue_member_t = unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> c_int;

#[repr(C)]
pub struct ecs_type_t {
    pub array: *mut ecs_id_t,
    pub count: i32,
}

pub const EcsPrimitiveType: i32 = 0;
pub const EcsBitmaskType: i32 = 1;
pub const EcsEnumType: i32 = 2;
pub const EcsStructType: i32 = 3;
pub const EcsArrayType: i32 = 4;
pub const EcsVectorType: i32 = 5;
pub const EcsOpaqueType: i32 = 6;
pub const EcsMapType: i32 = 7;
pub const EcsValueType: i32 = 8;

#[repr(C)]
pub struct EcsType {
    pub kind: i32,
    pub existing: bool,
    pub partial: bool,
}

pub const EcsBool: i32 = 1;
pub const EcsChar: i32 = 2;
pub const EcsByte: i32 = 3;
pub const EcsU8: i32 = 4;
pub const EcsU16: i32 = 5;
pub const EcsU32: i32 = 6;
pub const EcsU64: i32 = 7;
pub const EcsI8: i32 = 8;
pub const EcsI16: i32 = 9;
pub const EcsI32: i32 = 10;
pub const EcsI64: i32 = 11;
pub const EcsF32: i32 = 12;
pub const EcsF64: i32 = 13;
pub const EcsUPtr: i32 = 14;
pub const EcsIPtr: i32 = 15;
pub const EcsString: i32 = 16;
pub const EcsEntity: i32 = 17;
pub const EcsId: i32 = 18;

#[repr(C)]
pub struct EcsPrimitive {
    pub kind: i32,
}

#[repr(C)]
pub struct ecs_vec_t {
    pub array: *mut c_void,
    pub count: i32,
    pub size: i32,
}

#[repr(C)]
pub struct EcsStruct {
    pub members: ecs_vec_t,
}

#[repr(C)]
pub struct ecs_member_value_range_t {
    pub min: f64,
    pub max: f64,
}

#[repr(C)]
pub struct ecs_member_t {
    pub name: *const c_char,
    pub type_: ecs_entity_t,
    pub count: i32,
    pub offset: i32,
    pub unit: ecs_entity_t,
    pub use_offset: bool,
    pub range: ecs_member_value_range_t,
    pub error_range: ecs_member_value_range_t,
    pub warning_range: ecs_member_value_range_t,
    pub size: i32,
    pub member: ecs_entity_t,
}

#[repr(C)]
pub struct EcsEnum {
    pub underlying_type: ecs_entity_t,
}

#[repr(C)]
pub struct ecs_enum_constant_t {
    pub name: *const c_char,
    pub value: i64,
    pub value_unsigned: u64,
    pub constant: ecs_entity_t,
}

#[repr(C)]
pub struct ecs_bitmask_constant_t {
    pub name: *const c_char,
    pub value: u64,
    pub _unused: i64,
    pub constant: ecs_entity_t,
}

#[repr(C)]
pub struct EcsConstants {
    pub constants: *mut c_void,
    pub ordered_constants: ecs_vec_t,
}

#[repr(C)]
pub struct EcsArray {
    pub type_: ecs_entity_t,
    pub count: i32,
}

#[repr(C)]
pub struct EcsVector {
    pub type_: ecs_entity_t,
}

#[repr(C)]
pub struct EcsComponent {
    pub size: i32,
    pub alignment: i32,
}

pub const ECS_PAIR: u64 = 1u64 << 63;
pub const ECS_ID_FLAGS_MASK: u64 = 0xFFu64 << 60;
pub const ECS_COMPONENT_MASK: u64 = !ECS_ID_FLAGS_MASK;

pub type glue_log_t =
    unsafe extern "C" fn(level: i32, file: *const c_char, line: i32, msg: *const c_char);

unsafe extern "C" {
    pub fn glue_os_api(log: glue_log_t, abort: unsafe extern "C" fn());
    pub fn glue_children(
        world: *const ecs_world_t,
        parent: ecs_entity_t,
        out: *mut ecs_entity_t,
        cap: i32,
    ) -> i32;
    pub fn glue_roots(world: *mut ecs_world_t, out: *mut ecs_entity_t, cap: i32) -> i32;
    pub fn glue_with(
        world: *const ecs_world_t,
        id: ecs_id_t,
        out: *mut ecs_entity_t,
        cap: i32,
    ) -> i32;
    pub fn glue_layout(which: i32, align: i32) -> usize;
    pub fn glue_writer_new(world: *const ecs_world_t, ty: ecs_entity_t) -> *mut glue_writer_t;
    pub fn glue_writer_cursor(w: *mut glue_writer_t) -> *mut ecs_meta_cursor_t;
    pub fn glue_writer_commit(
        world: *mut ecs_world_t,
        w: *mut glue_writer_t,
        entity: ecs_entity_t,
        component: ecs_id_t,
        size: usize,
    );
    pub fn glue_writer_free(world: *const ecs_world_t, w: *mut glue_writer_t);
    pub fn glue_writer_ptr(w: *mut glue_writer_t) -> *const c_void;
    pub fn glue_table_dirty(
        world: *mut ecs_world_t,
        table: *mut ecs_table_t,
        out: *mut i32,
        cap: i32,
    ) -> i32;
    pub fn glue_opaque_as_type(world: *const ecs_world_t, ty: ecs_entity_t) -> ecs_entity_t;
    pub fn glue_opaque_serialize(
        world: *const ecs_world_t,
        ty: ecs_entity_t,
        ptr: *const c_void,
        value: glue_value_t,
        member: glue_member_t,
        ctx: *mut c_void,
    ) -> c_int;

    pub fn ecs_meta_push(c: *mut ecs_meta_cursor_t) -> c_int;
    pub fn ecs_meta_pop(c: *mut ecs_meta_cursor_t) -> c_int;
    pub fn ecs_meta_next(c: *mut ecs_meta_cursor_t) -> c_int;
    pub fn ecs_meta_member(c: *mut ecs_meta_cursor_t, name: *const c_char) -> c_int;
    pub fn ecs_meta_set_bool(c: *mut ecs_meta_cursor_t, v: bool) -> c_int;
    pub fn ecs_meta_set_int(c: *mut ecs_meta_cursor_t, v: i64) -> c_int;
    pub fn ecs_meta_set_uint(c: *mut ecs_meta_cursor_t, v: u64) -> c_int;
    pub fn ecs_meta_set_float(c: *mut ecs_meta_cursor_t, v: f64) -> c_int;
    pub fn ecs_meta_set_string(c: *mut ecs_meta_cursor_t, v: *const c_char) -> c_int;
    pub fn ecs_meta_set_entity(c: *mut ecs_meta_cursor_t, v: ecs_entity_t) -> c_int;
    pub fn ecs_meta_set_id(c: *mut ecs_meta_cursor_t, v: ecs_id_t) -> c_int;
    pub fn ecs_meta_set_null(c: *mut ecs_meta_cursor_t) -> c_int;
    // the read half: the value at the cursor's place, converted to the asked type.
    pub fn ecs_meta_get_bool(c: *const ecs_meta_cursor_t) -> bool;
    pub fn ecs_meta_get_int(c: *const ecs_meta_cursor_t) -> i64;
    pub fn ecs_meta_get_uint(c: *const ecs_meta_cursor_t) -> u64;
    pub fn ecs_meta_get_float(c: *const ecs_meta_cursor_t) -> f64;
    pub fn ecs_meta_get_string(c: *const ecs_meta_cursor_t) -> *const c_char;
    pub fn ecs_meta_get_entity(c: *const ecs_meta_cursor_t) -> ecs_entity_t;
    pub fn glue_member_offset(which: i32) -> usize;

    pub fn ecs_init() -> *mut ecs_world_t;
    pub fn ecs_fini(world: *mut ecs_world_t) -> c_int;
    pub fn ecs_script_run(
        world: *mut ecs_world_t,
        name: *const c_char,
        code: *const c_char,
        result: *mut c_void,
    ) -> c_int;
    pub fn ecs_lookup(world: *const ecs_world_t, path: *const c_char) -> ecs_entity_t;
    // finds a flecs_ecs component by its Rust symbol (flecs-describe's `describe_all`).
    pub fn ecs_lookup_symbol(
        world: *const ecs_world_t,
        symbol: *const c_char,
        lookup_as_path: bool,
        recursive: bool,
    ) -> ecs_entity_t;
    pub fn ecs_get_name(world: *const ecs_world_t, entity: ecs_entity_t) -> *const c_char;
    pub fn ecs_get_parent(world: *const ecs_world_t, entity: ecs_entity_t) -> ecs_entity_t;
    pub fn ecs_get_type(world: *const ecs_world_t, entity: ecs_entity_t) -> *const ecs_type_t;
    pub fn ecs_has_id(world: *const ecs_world_t, entity: ecs_entity_t, id: ecs_id_t) -> bool;
    pub fn ecs_owns_id(world: *const ecs_world_t, entity: ecs_entity_t, id: ecs_id_t) -> bool;
    pub fn ecs_get_id(
        world: *const ecs_world_t,
        entity: ecs_entity_t,
        id: ecs_id_t,
    ) -> *const c_void;
    pub fn ecs_get_typeid(world: *const ecs_world_t, id: ecs_id_t) -> ecs_entity_t;
    pub fn ecs_get_alive(world: *const ecs_world_t, entity: ecs_entity_t) -> ecs_entity_t;
    pub fn ecs_set_id(
        world: *mut ecs_world_t,
        entity: ecs_entity_t,
        id: ecs_id_t,
        size: usize,
        ptr: *const c_void,
    );
    pub fn ecs_get_target(
        world: *const ecs_world_t,
        entity: ecs_entity_t,
        rel: ecs_entity_t,
        index: i32,
    ) -> ecs_entity_t;
    pub fn ecs_set_lookup_path(
        world: *mut ecs_world_t,
        path: *const ecs_entity_t,
    ) -> *mut ecs_entity_t;
    pub fn ecs_is_alive(world: *const ecs_world_t, entity: ecs_entity_t) -> bool;
    pub fn ecs_new(world: *mut ecs_world_t) -> ecs_entity_t;
    pub fn ecs_delete(world: *mut ecs_world_t, entity: ecs_entity_t);
    pub fn ecs_add_id(world: *mut ecs_world_t, entity: ecs_entity_t, id: ecs_id_t);
    pub fn ecs_remove_id(world: *mut ecs_world_t, entity: ecs_entity_t, id: ecs_id_t);
    pub fn ecs_is_enabled_id(world: *const ecs_world_t, entity: ecs_entity_t, id: ecs_id_t)
    -> bool;
    /// Whether the world cannot be written straight away: a stage, a world in readonly mode (in a system or
    /// observer), or, on a host's 4.1.6, a deferred world (csrc/glue.c).
    pub fn glue_is_deferred(world: *const ecs_world_t) -> bool;
    /// Starts a batch of writes, applied together by `glue_batch_end`; returns the world to write the batch to (Flecs
    /// past 4.1.6 has no defer mode, so this is stage 0 there).
    pub fn glue_batch_begin(world: *mut ecs_world_t) -> *mut ecs_world_t;
    pub fn glue_batch_end(world: *mut ecs_world_t, batch: *mut ecs_world_t);
    pub fn ecs_get_mut_id(
        world: *const ecs_world_t,
        entity: ecs_entity_t,
        id: ecs_id_t,
    ) -> *mut c_void;
    pub fn ecs_modified_id(world: *mut ecs_world_t, entity: ecs_entity_t, id: ecs_id_t);
    pub fn glue_cursor_new(
        world: *const ecs_world_t,
        ty: ecs_entity_t,
        ptr: *mut c_void,
    ) -> *mut ecs_meta_cursor_t;
    pub fn glue_cursor_free(c: *mut ecs_meta_cursor_t);
    pub fn glue_set_string(slot: *mut *mut c_char, s: *const c_char, len: i32) -> c_int;
    // logical types (flecs-snapshot's logical.rs)
    pub fn glue_range_hooks(world: *mut ecs_world_t, range: ecs_entity_t) -> c_int;
    pub fn glue_member_type(world: *const ecs_world_t, member: ecs_entity_t) -> ecs_entity_t;
    pub fn glue_member_refine(
        world: *mut ecs_world_t,
        member: ecs_entity_t,
        has_range: bool,
        min: f64,
        max: f64,
        unit: ecs_entity_t,
    );
    pub fn ecs_get_table(world: *const ecs_world_t, entity: ecs_entity_t) -> *mut ecs_table_t;
    pub fn ecs_lookup_child(
        world: *const ecs_world_t,
        parent: ecs_entity_t,
        name: *const c_char,
    ) -> ecs_entity_t;
    pub fn ecs_set_name(
        world: *mut ecs_world_t,
        entity: ecs_entity_t,
        name: *const c_char,
    ) -> ecs_entity_t;

    pub static EcsFlecsCore: ecs_entity_t;
    pub static EcsChildOf: ecs_entity_t;
    pub static EcsIsA: ecs_entity_t;
    pub static EcsPrefab: ecs_entity_t;
    pub static EcsOrderedChildren: ecs_entity_t;
    pub static EcsOnInstantiate: ecs_entity_t;
    pub static EcsDontInherit: ecs_entity_t;
    pub static EcsInherit: ecs_entity_t;
    pub static EcsDontFragment: ecs_entity_t;
    pub static EcsWildcard: ecs_entity_t;
    pub static EcsDisabled: ecs_entity_t;
    pub static ECS_TOGGLE: ecs_id_t;
    pub static EcsCanToggle: ecs_entity_t;
    pub static EcsExclusive: ecs_entity_t;
    pub static EcsParentDepth: ecs_entity_t;
    pub static FLECS_IDEcsIdentifierID_: ecs_entity_t;
    pub static FLECS_IDEcsParentID_: ecs_entity_t;
    pub static FLECS_IDEcsComponentID_: ecs_entity_t;
    pub static FLECS_IDEcsTypeID_: ecs_entity_t;
    pub static FLECS_IDEcsPrimitiveID_: ecs_entity_t;
    pub static FLECS_IDEcsStructID_: ecs_entity_t;
    pub static FLECS_IDEcsEnumID_: ecs_entity_t;
    pub static FLECS_IDEcsBitmaskID_: ecs_entity_t;
    pub static FLECS_IDEcsConstantsID_: ecs_entity_t;
    pub static FLECS_IDEcsArrayID_: ecs_entity_t;
    pub static FLECS_IDEcsVectorID_: ecs_entity_t;
}

/// `ecs_pair(rel, tgt)`.
pub fn pair(rel: ecs_entity_t, tgt: ecs_entity_t) -> ecs_id_t {
    ECS_PAIR | ((rel & 0xFFFF_FFFF) << 32) | (tgt & 0xFFFF_FFFF)
}

pub fn is_pair(id: ecs_id_t) -> bool {
    id & ECS_ID_FLAGS_MASK == ECS_PAIR
}

/// The low 32 bits of each half of a pair (alive lookup gives the full entity).
pub fn pair_first(id: ecs_id_t) -> ecs_entity_t {
    (id & ECS_COMPONENT_MASK) >> 32
}

pub fn pair_second(id: ecs_id_t) -> ecs_entity_t {
    id & 0xFFFF_FFFF
}

// ---- Described types (crates/flecs-describe): Flecs meta and lifecycle hooks registered from a Rust description,
// through the glue_describe_* functions at the end of csrc/glue.c. ----

/// Opaque: the serializer an opaque type's serialize callback is given.
#[repr(C)]
pub struct ecs_serializer_t {
    _private: [u8; 0],
}

/// Opaque: passed to lifecycle hooks, never read here.
#[repr(C)]
pub struct ecs_type_info_t {
    _private: [u8; 0],
}

pub type ecs_xtor_t =
    unsafe extern "C" fn(ptr: *mut c_void, count: i32, ti: *const ecs_type_info_t);
pub type ecs_copy_t = unsafe extern "C" fn(
    dst: *mut c_void,
    src: *const c_void,
    count: i32,
    ti: *const ecs_type_info_t,
);
pub type ecs_move_t = unsafe extern "C" fn(
    dst: *mut c_void,
    src: *mut c_void,
    count: i32,
    ti: *const ecs_type_info_t,
);
pub type ecs_meta_serialize_t =
    unsafe extern "C" fn(ser: *const ecs_serializer_t, src: *const c_void) -> c_int;

unsafe extern "C" {
    pub fn glue_describe_primitive(kind: i32) -> ecs_entity_t;
    pub fn glue_describe_type(
        world: *mut ecs_world_t,
        parent: ecs_entity_t,
        name: *const c_char,
        size: i32,
        align: i32,
    ) -> ecs_entity_t;
    pub fn glue_describe_member(
        world: *mut ecs_world_t,
        type_: ecs_entity_t,
        name: *const c_char,
        member_type: ecs_entity_t,
        count: i32,
        offset: i32,
    ) -> c_int;
    pub fn glue_describe_array(
        world: *mut ecs_world_t,
        entity: ecs_entity_t,
        elem: ecs_entity_t,
        count: i32,
    ) -> ecs_entity_t;
    pub fn glue_describe_vector(
        world: *mut ecs_world_t,
        entity: ecs_entity_t,
        elem: ecs_entity_t,
    ) -> ecs_entity_t;
    pub fn glue_describe_enum(
        world: *mut ecs_world_t,
        entity: ecs_entity_t,
        underlying: ecs_entity_t,
    ) -> c_int;
    pub fn glue_describe_enum_constant(
        world: *mut ecs_world_t,
        type_: ecs_entity_t,
        name: *const c_char,
        value: i64,
        value_unsigned: u64,
    ) -> c_int;
    pub fn glue_describe_bitmask(world: *mut ecs_world_t, entity: ecs_entity_t) -> c_int;
    pub fn glue_describe_bitmask_constant(
        world: *mut ecs_world_t,
        type_: ecs_entity_t,
        name: *const c_char,
        value: u32,
    ) -> c_int;
    pub fn glue_describe_opaque(
        world: *mut ecs_world_t,
        type_: ecs_entity_t,
        as_type: ecs_entity_t,
        serialize: ecs_meta_serialize_t,
    ) -> ecs_entity_t;
    #[allow(clippy::too_many_arguments)]
    pub fn glue_describe_component(
        world: *mut ecs_world_t,
        name: *const c_char,
        size: i32,
        align: i32,
        ctor: ecs_xtor_t,
        dtor: ecs_xtor_t,
        copy: ecs_copy_t,
        move_: ecs_move_t,
        copy_ctor: ecs_copy_t,
        move_ctor: ecs_move_t,
        ctor_move_dtor: ecs_move_t,
        move_dtor: ecs_move_t,
    ) -> ecs_entity_t;
    pub fn glue_describe_ser_value(
        ser: *const ecs_serializer_t,
        type_: ecs_entity_t,
        ptr: *const c_void,
    ) -> c_int;
    pub fn glue_describe_ser_member(ser: *const ecs_serializer_t, name: *const c_char) -> c_int;
    pub fn glue_describe_ser_world(ser: *const ecs_serializer_t) -> *const ecs_world_t;
}

#[cfg(target_arch = "wasm32")]
mod wasm_libc {
    use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc as rs_realloc};
    use core::ffi::{c_char, c_void};

    const HEADER: usize = 16;

    fn layout(n: usize) -> Option<Layout> {
        Layout::from_size_align(n.checked_add(HEADER)?, HEADER).ok()
    }

    unsafe fn finish(base: *mut u8, n: usize) -> *mut c_void {
        if base.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `base` is a fresh block of at least HEADER bytes, 16-aligned.
        unsafe {
            base.cast::<usize>().write(n);
            base.add(HEADER).cast()
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc(n: usize) -> *mut c_void {
        let Some(l) = layout(n) else {
            return core::ptr::null_mut();
        };
        // SAFETY: the layout is non-zero sized (HEADER bytes at least).
        unsafe { finish(alloc(l), n) }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        let Some(n) = count.checked_mul(size) else {
            return core::ptr::null_mut();
        };
        let Some(l) = layout(n) else {
            return core::ptr::null_mut();
        };
        // SAFETY: as above.
        unsafe { finish(alloc_zeroed(l), n) }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(p: *mut c_void) {
        if p.is_null() {
            return;
        }
        // SAFETY: `p` came from this allocator, so the header sits HEADER bytes before it and holds the size.
        unsafe {
            let base = p.cast::<u8>().sub(HEADER);
            let n = base.cast::<usize>().read();
            dealloc(base, Layout::from_size_align_unchecked(n + HEADER, HEADER));
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
        if p.is_null() {
            // SAFETY: plain allocation.
            return unsafe { malloc(n) };
        }
        if layout(n).is_none() {
            return core::ptr::null_mut();
        }
        // SAFETY: `p` came from this allocator (see free); the new size fits a Layout (checked above).
        unsafe {
            let base = p.cast::<u8>().sub(HEADER);
            let old = base.cast::<usize>().read();
            let grown = rs_realloc(
                base,
                Layout::from_size_align_unchecked(old + HEADER, HEADER),
                n + HEADER,
            );
            finish(grown, n)
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn abort() -> ! {
        core::arch::wasm32::unreachable()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn exit(_code: i32) -> ! {
        core::arch::wasm32::unreachable()
    }

    /// The length of the longest prefix of `s` that strtod accepts as a decimal number (sign, digits, optional
    /// fraction, optional exponent), or as inf/infinity/nan; 0 if none. Hex floats are not accepted (Flecs passes
    /// only decimal literals).
    fn float_prefix(s: &[u8]) -> usize {
        let mut i = 0;
        if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        let word = |w: &[u8], at: usize| {
            s.len() >= at + w.len() && s[at..at + w.len()].eq_ignore_ascii_case(w)
        };
        if word(b"infinity", i) {
            return i + 8;
        }
        if word(b"inf", i) || word(b"nan", i) {
            return i + 3;
        }
        let digits = |mut j: usize| {
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
            }
            j
        };
        let int_end = digits(i);
        let mut end = int_end;
        let mut any = int_end > i;
        if end < s.len() && s[end] == b'.' {
            let frac_end = digits(end + 1);
            any |= frac_end > end + 1;
            end = frac_end;
        }
        if !any {
            return 0;
        }
        if end < s.len() && (s[end] == b'e' || s[end] == b'E') {
            let mut j = end + 1;
            if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
                j += 1;
            }
            let exp_end = digits(j);
            if exp_end > j {
                end = exp_end;
            }
        }
        end
    }

    /// Skips leading whitespace, finds the number and parses it with `parse`; sets `*end` like strtod.
    unsafe fn parse_c<T: Default>(
        s: *const c_char,
        end: *mut *mut c_char,
        parse: fn(&str) -> Option<T>,
    ) -> T {
        // SAFETY: `s` is a NUL-terminated C string (strtod's contract).
        let bytes = unsafe { core::ffi::CStr::from_ptr(s) }.to_bytes();
        let ws = bytes.iter().take_while(|c| matches!(c, b' ' | b'\t'..=b'\r')).count();
        let n = float_prefix(&bytes[ws..]);
        let text = core::str::from_utf8(&bytes[ws..ws + n]).unwrap_or("");
        let value = if n == 0 { None } else { parse(text) };
        if !end.is_null() {
            let used = if value.is_some() { ws + n } else { 0 };
            // SAFETY: `end` is a valid out pointer; `used` is within the string.
            unsafe { *end = s.add(used).cast_mut() };
        }
        value.unwrap_or_default()
    }

    fn parse_f64(t: &str) -> Option<f64> {
        t.parse::<f64>().ok()
    }

    fn parse_f32(t: &str) -> Option<f32> {
        t.parse::<f32>().ok()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
        // SAFETY: strtod's contract.
        unsafe { parse_c(s, end, parse_f64) }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn strtof(s: *const c_char, end: *mut *mut c_char) -> f32 {
        // SAFETY: strtof's contract.
        unsafe { parse_c(s, end, parse_f32) }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn atof(s: *const c_char) -> f64 {
        // SAFETY: atof's contract.
        unsafe { parse_c(s, core::ptr::null_mut(), parse_f64) }
    }
}

/// Opaque: Flecs's HTTP server, here only ever a never-started REST server from `glue_rest_new`.
#[cfg(feature = "rest")]
#[repr(C)]
pub struct ecs_http_server_t {
    _private: [u8; 0],
}

// The `rest` feature (native only): a read-only REST server over a world, fed one request at a time on the world's
// thread (csrc/glue.c).
#[cfg(feature = "rest")]
unsafe extern "C" {
    pub fn glue_rest_new(world: *mut ecs_world_t) -> *mut ecs_http_server_t;
    pub fn glue_rest_free(srv: *mut ecs_http_server_t);
    pub fn glue_rest_request(
        srv: *mut ecs_http_server_t,
        method: *const c_char,
        path: *const c_char,
        body: *const c_char,
        body_out: *mut *mut c_char,
        len_out: *mut i32,
        content_type_out: *mut *const c_char,
    ) -> i32;
    pub fn glue_free(ptr: *mut c_void);
}

#[cfg(all(test, feature = "rest"))]
mod rest_tests {
    use super::*;

    /// One request through the never-started server: (code, body).
    unsafe fn request(
        srv: *mut ecs_http_server_t,
        method: &core::ffi::CStr,
        path: &core::ffi::CStr,
    ) -> (i32, bool) {
        let (mut body, mut len, mut ct) = (core::ptr::null_mut(), 0i32, core::ptr::null());
        // SAFETY: a live server and NUL-terminated strings; the body is freed below.
        unsafe {
            let code = glue_rest_request(
                srv,
                method.as_ptr(),
                path.as_ptr(),
                core::ptr::null(),
                &mut body,
                &mut len,
                &mut ct,
            );
            let nonempty = !body.is_null() && len > 0;
            glue_free(body.cast());
            (code, nonempty)
        }
    }

    #[test]
    fn rest_serves_reads_and_refuses_writes() {
        // SAFETY: a world and a server made and freed here, on this thread.
        unsafe {
            let w = ecs_init();
            let srv = glue_rest_new(w);
            assert!(!srv.is_null());
            assert_eq!(request(srv, c"GET", c"/entity/flecs/core"), (200, true));
            assert_eq!(request(srv, c"PUT", c"/entity/made_by_rest").0, 403);
            assert_eq!(ecs_lookup(w, c"made_by_rest".as_ptr()), 0);
            assert_eq!(request(srv, c"DELETE", c"/entity/flecs/core").0, 403);
            assert_ne!(ecs_lookup(w, c"flecs.core".as_ptr()), 0);
            glue_rest_free(srv);
            ecs_fini(w);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn a_cursor_reads_back_each_member_kind_a_script_set() {
        let code = c"struct S(b: bool, i: i32, u: u64, f: f32, s: string, e: entity)\nt {}\nx { S: {b: true, i: -3, u: 7, f: 2.5, s: \"hi\", e: t} }\n";
        // SAFETY: a fresh world, NUL-terminated strings, and a cursor over x's own S, freed before the world.
        unsafe {
            let w = ecs_init();
            assert_eq!(ecs_script_run(w, c"t".as_ptr(), code.as_ptr(), core::ptr::null_mut()), 0);
            let (ty, x, t) = (
                ecs_lookup(w, c"S".as_ptr()),
                ecs_lookup(w, c"x".as_ptr()),
                ecs_lookup(w, c"t".as_ptr()),
            );
            let c = glue_cursor_new(w, ty, ecs_get_mut_id(w, x, ty));
            assert_eq!(ecs_meta_push(c), 0);
            let at = |m: &core::ffi::CStr| assert_eq!(ecs_meta_member(c, m.as_ptr()), 0);
            at(c"b");
            assert!(ecs_meta_get_bool(c));
            at(c"i");
            assert_eq!(ecs_meta_get_int(c), -3);
            at(c"u");
            assert_eq!(ecs_meta_get_uint(c), 7);
            at(c"f");
            assert_eq!(ecs_meta_get_float(c), 2.5);
            at(c"s");
            assert_eq!(core::ffi::CStr::from_ptr(ecs_meta_get_string(c)), c"hi");
            at(c"e");
            assert_eq!(ecs_meta_get_entity(c), t);
            glue_cursor_free(c);
            ecs_fini(w);
        }
    }

    #[test]
    fn layouts_match_c() {
        let rust = [
            (size_of::<ecs_type_t>(), align_of::<ecs_type_t>()),
            (size_of::<EcsType>(), align_of::<EcsType>()),
            (size_of::<EcsPrimitive>(), align_of::<EcsPrimitive>()),
            (size_of::<ecs_vec_t>(), align_of::<ecs_vec_t>()),
            (size_of::<EcsStruct>(), align_of::<EcsStruct>()),
            (size_of::<ecs_member_t>(), align_of::<ecs_member_t>()),
            (size_of::<EcsEnum>(), align_of::<EcsEnum>()),
            (size_of::<ecs_enum_constant_t>(), align_of::<ecs_enum_constant_t>()),
            (size_of::<ecs_bitmask_constant_t>(), align_of::<ecs_bitmask_constant_t>()),
            (size_of::<EcsConstants>(), align_of::<EcsConstants>()),
            (size_of::<EcsArray>(), align_of::<EcsArray>()),
            (size_of::<EcsVector>(), align_of::<EcsVector>()),
            (size_of::<EcsComponent>(), align_of::<EcsComponent>()),
        ];
        for (i, (size, align)) in rust.iter().enumerate() {
            // SAFETY: plain queries.
            let (c_size, c_align) = unsafe { (glue_layout(i as i32, 0), glue_layout(i as i32, 1)) };
            assert_eq!((*size, *align), (c_size, c_align), "struct {i}");
        }
        let offsets = [
            offset_of!(ecs_member_t, name),
            offset_of!(ecs_member_t, type_),
            offset_of!(ecs_member_t, count),
            offset_of!(ecs_member_t, offset),
            offset_of!(ecs_member_t, size),
        ];
        for (i, off) in offsets.iter().enumerate() {
            // SAFETY: plain query.
            assert_eq!(*off, unsafe { glue_member_offset(i as i32) }, "ecs_member_t field {i}");
        }
    }
}
