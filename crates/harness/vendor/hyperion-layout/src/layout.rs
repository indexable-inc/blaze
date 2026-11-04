//! Layout recipes and the layout hash (the Rust counterpart of Zig `plugin.layoutHash`).
//!
//! Every component field type implements [`Layout`]: a const [`Recipe`] tree (field names, offsets, sizes, alignments,
//! enum constants). The hash walks the tree and feeds exactly the bytes Zig's `hashInto` feeds to `Wyhash`, so
//! a Rust component and a Zig `extern struct` with the same layout have the same hash. The same tree drives
//! flecs reflection at run time (`hyperion-ecs`) and the by-name `auto` migration, so one declaration gives all three.

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicPtr, Ordering};
use hyperion_abi::table::Step;
use hyperion_wyhash::Wyhash;

/// Primitive kinds that flecs reflection understands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prim {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    /// A pointer or function pointer (8 bytes): opaque to reflection, its Zig `@typeName` is in the hash.
    Ptr,
    /// A NUL-terminated C string (flecs's `string`, a `char*` on flecs's allocator): reflected, so a script can write it.
    Str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnumKind {
    /// Fieldless enum with explicit constants.
    Enum,
    /// Entity id newtype (Zig `enum(u64) { _ }`): hashes as an enum with no constants, reflects as a flecs entity.
    Entity,
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub offset: usize,
    pub ty: &'static Recipe,
}

#[derive(Debug)]
pub enum Recipe {
    /// A scalar; `name` is the Zig `@typeName` ("u32", "f64", "bool", ...).
    Prim {
        name: &'static str,
        kind: Prim,
        size: usize,
        align: usize,
    },
    Array {
        len: usize,
        elem: &'static Recipe,
        size: usize,
        align: usize,
    },
    Struct {
        fields: &'static [Field],
        size: usize,
        align: usize,
    },
    Enum {
        kind: EnumKind,
        consts: &'static [(&'static str, i64)],
        tag: &'static Recipe,
        size: usize,
        align: usize,
    },
    /// Data whose bytes are not its value (a `String`, a `Vec`, an `Option`, an enum with data, an `Arc`): reflected as a
    /// flecs opaque type, which reads as `form` through `serialize`. `shape` is what the hash, size and alignment are of.
    Opaque { shape: &'static Recipe, form: Form, serialize: Serialize },
}

/// A flecs opaque type's `serialize` (`ecs_meta_serialize_t`: the serializer, then the value), always [`serialize`]`::<T>`.
pub type Serialize = unsafe extern "C" fn(ser: *const c_void, src: *const c_void) -> c_int;

/// How an opaque type reads. Each accessor takes a pointer to a live value of the type.
#[derive(Debug)]
pub enum Form {
    /// Words, handed to `out` (flecs's `string`).
    Text { get: unsafe fn(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int },
    /// Data with no fixed shape (a JSON value, a tree), as JSON text handed to `out`: flecs's `string`, marked JSON.
    Json { get: unsafe fn(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int },
    /// A run of `elem`s (first, count): a flecs vector of it.
    List { elem: &'static Recipe, get: unsafe fn(src: *const u8) -> (*const u8, usize) },
    /// One `elem` or none (null): a flecs vector of at most one, marked optional.
    Maybe { elem: &'static Recipe, get: unsafe fn(src: *const u8) -> *const u8 },
    /// An `elem` held elsewhere (an `Arc`): reads as the `elem` itself.
    Ref { elem: &'static Recipe, get: unsafe fn(src: *const u8) -> *const u8 },
    /// One of `variants` (which, and its data or null): a flecs struct with a member per variant, marked as a choice, of
    /// which a value holds the one it is.
    OneOf { variants: &'static [Variant], get: unsafe fn(src: *const u8) -> (usize, *const u8) },
}

/// A variant of an enum with data: its name and what it holds (none: nothing).
#[derive(Debug)]
pub struct Variant {
    pub name: &'static str,
    pub data: Option<&'static Recipe>,
}

/// The flecs layer's serializer for an opaque recipe (`hyperion-ecs` sets it when it reflects the first opaque type); each
/// image has its own, and only the host, which registers every component, sets and calls it.
pub type SerializeRecipe = unsafe fn(ser: *const c_void, recipe: &'static Recipe, src: *const c_void) -> c_int;

static SERIALIZER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Make `f` the serializer [`serialize`] hands every opaque value to.
pub fn set_serializer(f: SerializeRecipe) {
    SERIALIZER.store(f as *mut (), Ordering::Release);
}

/// The flecs `serialize` of opaque type `T`: flecs calls it with no context, so each type has its own, which names its recipe.
///
/// # Safety
/// `ser` is a live flecs serializer and `src` a live value of `T`.
pub unsafe extern "C" fn serialize<T: Layout>(ser: *const c_void, src: *const c_void) -> c_int {
    let f = SERIALIZER.load(Ordering::Acquire);
    if f.is_null() {
        return -1;
    }
    // SAFETY: only `set_serializer` stores here, a `SerializeRecipe`.
    let f: SerializeRecipe = unsafe { core::mem::transmute::<*mut (), SerializeRecipe>(f) };
    // SAFETY: the caller's contract.
    unsafe { f(ser, T::RECIPE, src) }
}

impl Recipe {
    pub const fn size(&self) -> usize {
        match self {
            Recipe::Prim { size, .. } | Recipe::Array { size, .. } | Recipe::Struct { size, .. } | Recipe::Enum { size, .. } => *size,
            Recipe::Opaque { shape, .. } => shape.size(),
        }
    }
    pub const fn align(&self) -> usize {
        match self {
            Recipe::Prim { align, .. } | Recipe::Array { align, .. } | Recipe::Struct { align, .. } | Recipe::Enum { align, .. } => *align,
            Recipe::Opaque { shape, .. } => shape.align(),
        }
    }
    /// Layout hash of this recipe (see module docs).
    pub const fn hash(&self) -> u64 {
        feed(Wyhash::new(0), self).finish()
    }
}

const fn feed(mut h: Wyhash, r: &Recipe) -> Wyhash {
    match r {
        Recipe::Prim { name, size, align, .. } => {
            h = h.update(name.as_bytes());
            h = h.update_u64(*size as u64);
            h.update_u64(*align as u64)
        }
        Recipe::Array { len, elem, size, align } => {
            h = h.update(b"array");
            h = h.update_u64(*len as u64);
            h = feed(h, elem);
            h = h.update_u64(*size as u64);
            h.update_u64(*align as u64)
        }
        Recipe::Struct { fields, size, align } => {
            h = h.update(b"struct");
            let mut i = 0;
            while i < fields.len() {
                h = h.update(fields[i].name.as_bytes());
                h = h.update_u64(fields[i].offset as u64);
                h = feed(h, fields[i].ty);
                i += 1;
            }
            h = h.update_u64(*size as u64);
            h.update_u64(*align as u64)
        }
        Recipe::Enum { consts, tag, size, align, .. } => {
            h = h.update(b"enum");
            let mut i = 0;
            while i < consts.len() {
                h = h.update(consts[i].0.as_bytes());
                h = h.update(&consts[i].1.to_le_bytes());
                i += 1;
            }
            h = feed(h, tag);
            h = h.update_u64(*size as u64);
            h.update_u64(*align as u64)
        }
        Recipe::Opaque { shape, .. } => feed(h, shape),
    }
}

/// A type that may appear as a component or a component field. Implemented for the scalars, arrays, and by
/// [`component!`](crate::component) / [`layout_enum!`](crate::layout_enum) for user types. Never implement by hand for a type
/// whose layout you did not check: the recipe is what the hash covers.
pub trait Layout: 'static {
    const RECIPE: &'static Recipe;
    const HASH: u64 = Self::RECIPE.hash();
    /// Migration chain (oldest first, ending at this type); empty for a type declared without `chain:`. Carried here, not on `Component`, so an
    /// api crate's `ApiType` (which only knows `Layout`) hands it to the module manifest.
    const CHAIN: &'static [Step] = &[];
}

macro_rules! prim {
    ($($t:ty => $k:ident),*) => { $(
        impl Layout for $t {
            const RECIPE: &'static Recipe = &Recipe::Prim { name: stringify!($t), kind: Prim::$k, size: size_of::<$t>(), align: align_of::<$t>() };
        }
    )* };
}
prim!(bool => Bool, u8 => U8, u16 => U16, u32 => U32, u64 => U64, i8 => I8, i16 => I16, i32 => I32, i64 => I64, f32 => F32, f64 => F64);

impl<T: Layout, const N: usize> Layout for [T; N] {
    const RECIPE: &'static Recipe = &Recipe::Array { len: N, elem: T::RECIPE, size: size_of::<Self>(), align: align_of::<Self>() };
}

/// `stringify!` of a raw identifier gives "r#loop"; the field name Zig hashes is "loop".
pub const fn raw_name(s: &'static str) -> &'static str {
    let b = s.as_bytes();
    if b.len() > 2 && b[0] == b'r' && b[1] == b'#' {
        // SAFETY: dropping an ASCII two-byte prefix of a valid UTF-8 string leaves valid UTF-8.
        unsafe { ::core::str::from_utf8_unchecked(b.split_at(2).1) }
    } else {
        s
    }
}

/// Sum of the sizes of a struct's fields: equal to the struct size iff there is no implicit padding.
pub const fn fields_size(fields: &[Field]) -> usize {
    let mut s = 0;
    let mut i = 0;
    while i < fields.len() {
        s += fields[i].ty.size();
        i += 1;
    }
    s
}

/// Entity id field (flecs `ecs_entity_t`), hashes like Zig `components.Entity` (`enum(u64) { _ }`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Entity(pub u64);

impl Layout for Entity {
    const RECIPE: &'static Recipe = &Recipe::Enum { kind: EnumKind::Entity, consts: &[], tag: <u64 as Layout>::RECIPE, size: 8, align: 8 };
}

// den: the types den-native's host-owned components hold. Each is an opaque type (reflection reads it and never writes it)
// whose `shape` still carries what its layout depends on, so a change to an element type changes the hash of every component
// holding it, and the hash is the one these types had before they were reflected.

impl Layout for usize {
    const RECIPE: &'static Recipe = &Recipe::Prim {
        name: "usize",
        kind: if size_of::<usize>() == 8 { Prim::U64 } else { Prim::U32 },
        size: size_of::<usize>(),
        align: align_of::<usize>(),
    };
}

impl Layout for isize {
    const RECIPE: &'static Recipe = &Recipe::Prim {
        name: "isize",
        kind: if size_of::<isize>() == 8 { Prim::I64 } else { Prim::I32 },
        size: size_of::<isize>(),
        align: align_of::<isize>(),
    };
}

/// Read as a one-character string: a script writing an arbitrary `u32` could make an invalid `char`, so it is never written.
impl Layout for char {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Prim { name: "char", kind: Prim::Ptr, size: 4, align: 4 },
        form: Form::Text { get: char_text },
        serialize: serialize::<char>,
    };
}

unsafe fn char_text(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int {
    // SAFETY: the caller's contract: a live `char`.
    let c = unsafe { *src.cast::<char>() };
    out(c.encode_utf8(&mut [0; 4]))
}

/// Heap data on the process allocator, which every image shares; only the host registers a component holding it.
impl Layout for alloc::string::String {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Prim {
            name: "String",
            kind: Prim::Ptr,
            size: size_of::<alloc::string::String>(),
            align: align_of::<alloc::string::String>(),
        },
        form: Form::Text { get: string_text },
        serialize: serialize::<alloc::string::String>,
    };
}

unsafe fn string_text(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int {
    // SAFETY: the caller's contract: a live `String`.
    out(unsafe { &*src.cast::<alloc::string::String>() })
}

impl<T: Layout> Layout for alloc::vec::Vec<T> {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Struct {
            fields: &[Field { name: "Vec", offset: 0, ty: &Recipe::Array { len: 0, elem: T::RECIPE, size: 0, align: 1 } }],
            size: size_of::<Self>(),
            align: align_of::<Self>(),
        },
        form: Form::List { elem: T::RECIPE, get: vec_items::<T> },
        serialize: serialize::<Self>,
    };
}

unsafe fn vec_items<T>(src: *const u8) -> (*const u8, usize) {
    // SAFETY: the caller's contract: a live `Vec<T>`.
    let v = unsafe { &*src.cast::<alloc::vec::Vec<T>>() };
    (v.as_ptr().cast(), v.len())
}

impl<A: Layout, B: Layout> Layout for (A, B) {
    const RECIPE: &'static Recipe = &Recipe::Struct {
        fields: &[
            Field { name: "0", offset: core::mem::offset_of!(Self, 0), ty: A::RECIPE },
            Field { name: "1", offset: core::mem::offset_of!(Self, 1), ty: B::RECIPE },
        ],
        size: size_of::<Self>(),
        align: align_of::<Self>(),
    };
}

impl<T: Layout> Layout for Option<T> {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Struct {
            fields: &[Field { name: "Option", offset: 0, ty: &Recipe::Array { len: 0, elem: T::RECIPE, size: 0, align: 1 } }],
            size: size_of::<Self>(),
            align: align_of::<Self>(),
        },
        form: Form::Maybe { elem: T::RECIPE, get: option_value::<T> },
        serialize: serialize::<Self>,
    };
}

unsafe fn option_value<T>(src: *const u8) -> *const u8 {
    // SAFETY: the caller's contract: a live `Option<T>`.
    unsafe { &*src.cast::<Option<T>>() }.as_ref().map_or(core::ptr::null(), |v| core::ptr::from_ref(v).cast())
}
