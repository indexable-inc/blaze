//! A debug form for every type, in place of `{:?}` and `#[derive(Debug)]`: read from the type
//! by compile-time reflection (`core::mem::type_info`), with no derive and no bound.
//!
//! How it works. For each type the walk reaches, a constant [`Form`] is computed at compile
//! time from the type's reflection: a leaf (integer, float, `bool`, `char`, string, address),
//! a reference, an array, or parts (a struct's, a tuple's or each enum variant's fields, each
//! with its name, its offset and the vtable of [`Reflect`] for its type, found from its
//! `TypeId`). At run time one non-generic interpreter ([`walk`]) reads the value through that
//! form; a field is written by calling its type's own `Reflect` through the vtable, so the
//! field's type is known again there and its own form is read. A type supplies its own form by
//! implementing [`DebugForm`], found from its `TypeId` before reflection is asked of anything
//! else, so the fields of a type with its own form are never read.
//!
//! What is refused at compile time, naming the type: a union, a closure, a `dyn` other than
//! through [`DebugForm`], `f16` and `f128`, and a field whose type is not sized. Each such
//! type, or one holding it, implements [`DebugForm`]. `core::num::NonZero` is refused too:
//! the pinned nightly's reflection gives it no size (seen on `Option<NonZero<u32>>`), so no
//! vtable is found for it, and its own form could not be found either.
//!
//! What it cannot know. Reflection on the pinned nightly names no enum variant's
//! discriminant, so the active variant is taken to be the one whose index equals the value's
//! discriminant: right for every enum that sets no discriminant by hand. An enum that does
//! (`Red = 1`) implements [`DebugForm`]; a discriminant past the last variant stops with a
//! panic naming the type. A `#[repr(packed)]` struct also implements [`DebugForm`]: its fields
//! are not aligned, and the walk reads a field in place.
//!
//! Strings and characters are escaped as [`crate::escape::rust`] does: the same as `{:?}` on
//! ASCII; other characters are copied as they are, where `{:?}` escapes the non-printable
//! ones.
//!
//! Two traps of the pinned nightly, kept clear of. Reflection's `info()` panics the compiler
//! on every tuple (crates/blaze-std/src/own/raw.rs says where), so a tuple is known by its
//! name (`(`) and never asked of `info()`. And a reference's pointee is asked its size before
//! its kind, so a pointee that is a tuple is never asked of `info()` either.

use core::alloc::Allocator;
use core::any::TypeId;
use core::marker::PhantomData;
use core::mem::type_info::{self, TypeKind};
use core::ptr::{self, DynMetadata, NonNull};

use blaze_own::{Arc, BTreeMap, BTreeSet, Box, FixedString, Rc, SortedMap, SortedSet, String, Vec, VecDeque};

use crate::put::Put;
use crate::write::Writes;

/// A type's own debug form, used in place of the one reflection reads: for a type whose
/// fields are not what a reader should see (a collection, a smart pointer, an enum with set
/// discriminants).
pub trait DebugForm {
    fn debug_form(&self, out: &mut dyn Put);
}

/// Every sized type's debug form, as a vtable the walk finds from a field's `TypeId`.
trait Reflect {
    fn reflect(&self, out: &mut dyn Put);
}

impl<T> Reflect for T {
    #[inline]
    fn reflect(&self, out: &mut dyn Put) {
        sized(self, out);
    }
}

/// `x` in `{:?}`'s layout.
#[inline]
pub(crate) fn debug<T: ?Sized>(x: &T, out: &mut dyn Put) {
    // A reference is sized whatever it points at, and its form follows it.
    sized::<&T>(&x, out);
}

/// A sized value's form: its own [`DebugForm`], else the one reflection reads.
fn sized<T>(x: &T, out: &mut dyn Put) {
    // One constant per type, its own form found first: a type with one is never reflected on,
    // so a field reflection cannot read (`NonNull`'s pattern type in a `Vec`) is never asked.
    let form: Form = const { form::<T>() };
    let variant = match form {
        Form::Enum { .. } => variant_index(x),
        _ => 0,
    };
    // SAFETY: `form` was read from `T`'s own reflection, and `x` is a live, aligned `T`.
    unsafe { walk(form, ptr::from_ref(x).cast::<u8>(), variant, out) }
}

/// The index of `x`'s variant: its discriminant, read at the discriminant's own width.
fn variant_index<T>(x: &T) -> u128 {
    let discriminant = core::intrinsics::discriminant_value(x);
    let at = ptr::from_ref(&discriminant).cast::<u8>();
    // SAFETY: `at` points at the discriminant, of the size matched, which is an integer of
    // that width.
    unsafe {
        match size_of_val(&discriminant) {
            1 => u128::from(at.read()),
            2 => u128::from(at.cast::<u16>().read()),
            4 => u128::from(at.cast::<u32>().read()),
            8 => u128::from(at.cast::<u64>().read()),
            16 => at.cast::<u128>().read(),
            _ => unreachable!("a discriminant is an integer of 1, 2, 4, 8 or 16 bytes"),
        }
    }
}

/// A field: its name (a tuple's is its index), its offset in its parent, and its type's
/// [`Reflect`].
#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    offset: usize,
    write: DynMetadata<dyn Reflect>,
}

/// A struct's or a variant's name and fields; `named` when written `{ a: .. }`.
#[derive(Clone, Copy)]
struct Parts {
    name: &'static str,
    fields: &'static [Field],
    named: bool,
}

/// What a type is, as the walk reads it.
#[derive(Clone, Copy)]
enum Form {
    /// The type's own [`DebugForm`].
    Own(DynMetadata<dyn DebugForm>),
    Bool,
    Char,
    Unsigned(usize),
    Signed(usize),
    Float(usize),
    /// `&str` or `&mut str`.
    Str,
    /// `&[E]` or `&mut [E]`.
    Slice { element: DynMetadata<dyn Reflect>, stride: usize },
    /// `[E; len]`.
    Array { element: DynMetadata<dyn Reflect>, stride: usize, len: usize },
    /// `&U` or `&mut U`, `U` sized.
    To(DynMetadata<dyn Reflect>),
    /// A raw pointer or a function pointer: written as its address, as `{:?}` does.
    Address,
    Tuple(&'static [Field]),
    Struct(Parts),
    Enum { type_name: &'static str, variants: &'static [Parts] },
}

/// # Safety
///
/// `at` points at a live value whose type `form` was read from; for an enum, `variant` is the
/// value's discriminant.
unsafe fn walk(form: Form, at: *const u8, variant: u128, out: &mut dyn Put) {
    // SAFETY (every read below): the form says what type is at `at`, and it is aligned for it.
    unsafe {
        match form {
            Form::Own(own) => {
                let value: &dyn DebugForm = &*ptr::from_raw_parts(at.cast::<()>(), own);
                value.debug_form(out);
            }
            Form::Bool =>_ = out.bool(at.cast::<bool>().read()),
            Form::Char => char_debug(at.cast::<char>().read(), out),
            Form::Unsigned(1) => _ = out.int(at.read()),
            Form::Unsigned(2) => _ = out.int(at.cast::<u16>().read()),
            Form::Unsigned(4) => _ = out.int(at.cast::<u32>().read()),
            Form::Unsigned(8) => _ = out.int(at.cast::<u64>().read()),
            Form::Unsigned(_) => _ = out.int(at.cast::<u128>().read()),
            Form::Signed(1) => _ = out.int(at.cast::<i8>().read()),
            Form::Signed(2) => _ = out.int(at.cast::<i16>().read()),
            Form::Signed(4) => _ = out.int(at.cast::<i32>().read()),
            Form::Signed(8) => _ = out.int(at.cast::<i64>().read()),
            Form::Signed(_) => _ = out.int(at.cast::<i128>().read()),
            Form::Float(4) => _ = out.float_debug(at.cast::<f32>().read()),
            Form::Float(_) => _ = out.float_debug(at.cast::<f64>().read()),
            Form::Str => _ = out.escaped(&*at.cast::<*const str>().read()),
            Form::Slice { element, stride } => {
                let slice = at.cast::<*const [u8]>().read();
                elements(slice.cast::<u8>(), slice.len(), stride, element, out);
            }
            Form::Array { element, stride, len } => elements(at, len, stride, element, out),
            Form::To(write) => field(at.cast::<*const u8>().read(), write, out),
            Form::Address => {
                out.put_str("0x");
                out.hex(at.cast::<usize>().read());
            }
            Form::Tuple(fields) => {
                out.put_byte(b'(');
                list(at, fields, out);
                if fields.len() == 1 {
                    out.put_byte(b',');
                }
                out.put_byte(b')');
            }
            Form::Struct(parts) => self::parts(at, parts, out),
            Form::Enum { type_name, variants } => match usize::try_from(variant).ok().and_then(|i| variants.get(i)) {
                Some(parts) => self::parts(at, *parts, out),
                _ => panic!(
                    "blaze-text: a `{type_name}` has discriminant {variant}, which names no variant by index: an enum with \
                     discriminants set by hand implements `blaze_text::DebugForm`"
                ),
            },
        }
    }
}

/// `[a, b]` from `len` elements `stride` bytes apart.
///
/// # Safety
///
/// `at` points at `len` live values of the type `write` is for, `stride` bytes apart.
unsafe fn elements(at: *const u8, len: usize, stride: usize, write: DynMetadata<dyn Reflect>, out: &mut dyn Put) {
    out.put_byte(b'[');
    for i in 0..len {
        if i > 0 {
            out.put_str(", ");
        }
        // SAFETY: element `i` of `len`, per the caller.
        unsafe { field(at.add(i * stride), write, out) };
    }
    out.put_byte(b']');
}

/// `Name`, `Name(a, b)` or `Name { x: a, y: b }`.
///
/// # Safety
///
/// `at` points at a live value with these parts.
unsafe fn parts(at: *const u8, parts: Parts, out: &mut dyn Put) {
    out.put_str(parts.name);
    if parts.fields.is_empty() {
        return;
    }
    match parts.named {
        true => {
            out.put_str(" { ");
            // SAFETY: per the caller.
            unsafe { list(at, parts.fields, out) };
            out.put_str(" }");
        }
        false => {
            out.put_byte(b'(');
            // SAFETY: per the caller.
            unsafe { list(at, parts.fields, out) };
            out.put_byte(b')');
        }
    }
}

/// The fields, `, `-separated, each named when it has a name that is not its index.
///
/// # Safety
///
/// `at` points at a live value with these fields.
unsafe fn list(at: *const u8, fields: &[Field], out: &mut dyn Put) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.put_str(", ");
        }
        if !is_index(f.name) {
            out.put_str(field_name(f.name));
            out.put_str(": ");
        }
        // SAFETY: the field's offset in a live value, per the caller.
        unsafe { field(at.add(f.offset), f.write, out) };
    }
}

/// The name a field is written with; the tests plant a bug here.
#[inline(always)]
fn field_name(name: &'static str) -> &'static str {
    #[cfg(test)]
    {
        crate::plant::field_name(name)
    }
    #[cfg(not(test))]
    {
        name
    }
}

/// One value through its type's [`Reflect`].
///
/// # Safety
///
/// `at` points at a live, aligned value of the type `write` is for.
unsafe fn field(at: *const u8, write: DynMetadata<dyn Reflect>, out: &mut dyn Put) {
    // SAFETY: per the caller; `write` is that type's vtable for `Reflect`.
    let value: &dyn Reflect = unsafe { &*ptr::from_raw_parts(at.cast::<()>(), write) };
    value.reflect(out);
}

fn is_index(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// A `char` as `{:?}` writes it: `'a'`, `'\''`, `'\n'`, `'\u{1b}'`.
fn char_debug(c: char, out: &mut dyn Put) {
    out.put_byte(b'\'');
    match c {
        '\'' => out.put_str("\\'"),
        '"' => out.put_byte(b'"'),
        _ => {
            let mut bytes = [0u8; 4];
            crate::escape::rust(c.encode_utf8(&mut bytes), out);
        }
    }
    out.put_byte(b'\'');
}

// What runs at compile time: reading a type's form from its reflection.

/// `T`'s form.
#[rustc_comptime]
fn form<T>() -> Form {
    let id = type_info::of::<T>();
    let name = core::intrinsics::reflection::type_name::<T>();
    if let Some(own) = id.trait_info_of::<dyn DebugForm>() {
        return Form::Own(own.get_vtable());
    }
    // A tuple is never asked of `info()` (the module says why); its name starts with `(`.
    if name.as_bytes()[0] == b'(' {
        return Form::Tuple(fields_of(id, 0, name));
    }
    form_of(id, name)
}

/// The form of the sized type `id`, not a tuple.
#[rustc_comptime]
fn form_of(id: TypeId, name: &'static str) -> Form {
    let size = match id.size() {
        Some(size) => size,
        None => refuse(name, "is not sized"),
    };
    match id.info().kind {
        TypeKind::Bool => Form::Bool,
        TypeKind::Char => Form::Char,
        TypeKind::Int if id.is_signed() => Form::Signed(size),
        TypeKind::Int => Form::Unsigned(size),
        TypeKind::Float if size == 4 || size == 8 => Form::Float(size),
        TypeKind::Float => refuse(name, "is a float of a width blaze-text writes no debug form for"),
        TypeKind::Reference => match id.points_to() {
            Some(pointee) => pointee_form(pointee, name),
            None => refuse(name, "is a reference reflection names no pointee for"),
        },
        TypeKind::Pointer | TypeKind::FnPtr => Form::Address,
        TypeKind::Array => match id.element_ty() {
            Some(element) => {
                let len = id.array_len();
                Form::Array { element: reflect_of(element, name), stride: if len == 0 { 0 } else { size / len }, len }
            }
            None => refuse(name, "is an array reflection names no element type for"),
        },
        TypeKind::Struct => {
            let fields = fields_of(id, 0, name);
            Form::Struct(Parts { name: short_name(name), fields, named: named(fields) })
        }
        TypeKind::Enum => {
            let count = id.variants();
            Form::Enum { type_name: name, variants: variants_of(id, count, name) }
        }
        _ => refuse(name, "is a union, a closure, a `dyn` or another type reflection gives no fields for"),
    }
}

/// The form of a reference to `pointee`.
#[rustc_comptime]
fn pointee_form(pointee: TypeId, name: &'static str) -> Form {
    // A sized pointee (perhaps a tuple, so asked its size first) is written by its own form.
    if pointee.size().is_some() {
        return Form::To(reflect_of(pointee, name));
    }
    match pointee.info().kind {
        TypeKind::Str(_) => Form::Str,
        TypeKind::Slice => match pointee.element_ty() {
            Some(element) => match element.size() {
                Some(stride) => Form::Slice { element: reflect_of(element, name), stride },
                None => refuse(name, "is a slice of unsized elements"),
            },
            None => refuse(name, "is a slice reflection names no element type for"),
        },
        _ => refuse(name, "points at a `dyn` or another unsized type: implement `blaze_text::DebugForm` for it"),
    }
}

/// The vtable of [`Reflect`] for the sized type `id`, reached from the type `name`.
#[rustc_comptime]
fn reflect_of(id: TypeId, name: &'static str) -> DynMetadata<dyn Reflect> {
    match id.trait_info_of::<dyn Reflect>() {
        Some(found) => found.get_vtable(),
        None => refuse(name, "holds a value of a type that is not sized"),
    }
}

/// Whether the fields are written `{ name: .. }`: a tuple's and a tuple struct's are named by
/// their index.
#[rustc_comptime]
fn named(fields: &[Field]) -> bool {
    !fields.is_empty() && !is_index_const(fields[0].name)
}

#[rustc_comptime]
fn is_index_const(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            return false;
        }
        i += 1;
    }
    true
}

/// The fields of `id`'s variant `variant`, as a slice kept in the binary.
#[rustc_comptime]
fn fields_of(id: TypeId, variant: usize, parent: &'static str) -> &'static [Field] {
    let count = id.fields(variant);
    if count == 0 {
        return &[];
    }
    let at = allocate::<Field>(count);
    let mut i = 0;
    while i < count {
        let field = id.field(variant, i);
        let name = field.name();
        // SAFETY: `at` holds `count` fields' room; `i < count`.
        unsafe { at.add(i).write(Field { name, offset: field.offset(), write: reflect_of(field.type_id(), parent) }) };
        i += 1;
    }
    keep(at, count)
}

/// Each variant's name and fields.
#[rustc_comptime]
fn variants_of(id: TypeId, count: usize, name: &'static str) -> &'static [Parts] {
    if count == 0 {
        return &[];
    }
    let at = allocate::<Parts>(count);
    let mut i = 0;
    while i < count {
        let fields = fields_of(id, i, name);
        // SAFETY: `at` holds `count` variants' room; `i < count`.
        unsafe { at.add(i).write(Parts { name: id.variant(i).name(), fields, named: named(fields) }) };
        i += 1;
    }
    keep(at, count)
}

/// Room for `count` values in the compile-time heap.
#[rustc_comptime]
fn allocate<T>(count: usize) -> *mut T {
    // SAFETY: a fresh compile-time allocation of `count` values' size at their alignment.
    unsafe { core::intrinsics::const_allocate(size_of::<T>() * count, align_of::<T>()).cast::<T>() }
}

/// The `count` values written at `at`, kept in the binary as a slice.
#[rustc_comptime]
fn keep<T>(at: *mut T, count: usize) -> &'static [T] {
    // SAFETY: `at` is a compile-time allocation from `allocate`, its `count` values all
    // written; made global, it is immutable and lives for the program.
    unsafe {
        let kept = core::intrinsics::const_make_global(at.cast::<u8>()).cast::<T>();
        core::slice::from_raw_parts(kept, count)
    }
}

/// A type's name as `#[derive(Debug)]` writes it: no path, no generic arguments.
#[rustc_comptime]
fn short_name(name: &'static str) -> &'static str {
    let bytes = name.as_bytes();
    let mut end = 0;
    while end < bytes.len() && bytes[end] != b'<' {
        end += 1;
    }
    let mut start = end;
    while start > 0 && bytes[start - 1] != b':' {
        start -= 1;
    }
    let (_, tail) = bytes.split_at(start);
    let (short, _) = tail.split_at(end - start);
    match core::str::from_utf8(short) {
        Ok(short) => short,
        Err(_) => name,
    }
}

// Own forms: types whose fields are not what a reader should see.

/// `[a, b]`.
fn seq<'a, T: 'a>(items: impl Iterator<Item = &'a T>, out: &mut dyn Put) {
    out.put_byte(b'[');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.put_str(", ");
        }
        out.debug(item);
    }
    out.put_byte(b']');
}

/// `{a, b}`.
fn set<'a, T: 'a>(items: impl Iterator<Item = &'a T>, out: &mut dyn Put) {
    out.put_byte(b'{');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.put_str(", ");
        }
        out.debug(item);
    }
    out.put_byte(b'}');
}

/// `{k: v, k: v}`.
fn map<'a, K: 'a, V: 'a>(entries: impl Iterator<Item = (&'a K, &'a V)>, out: &mut dyn Put) {
    out.put_byte(b'{');
    for (i, (k, v)) in entries.enumerate() {
        if i > 0 {
            out.put_str(", ");
        }
        out.debug(k).str(": ").debug(v);
    }
    out.put_byte(b'}');
}

impl<A: Allocator> DebugForm for String<A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.escaped(self.as_str());
    }
}

impl<const N: usize> DebugForm for FixedString<N> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.escaped(self.as_str());
    }
}

impl<T, A: Allocator> DebugForm for Vec<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        seq(self.as_slice().iter(), out);
    }
}

impl<T, A: Allocator> DebugForm for VecDeque<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        seq(self.iter(), out);
    }
}

impl<T: ?Sized, A: Allocator> DebugForm for Box<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.debug(&**self);
    }
}

impl<T: ?Sized, A: Allocator> DebugForm for Arc<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.debug(&**self);
    }
}

impl<T: ?Sized, A: Allocator> DebugForm for Rc<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.debug(&**self);
    }
}

impl<K: Ord, V, A: Allocator> DebugForm for SortedMap<K, V, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        map(self.as_slice().iter().map(|(k, v)| (k, v)), out);
    }
}

impl<K, V, A: Allocator> DebugForm for BTreeMap<K, V, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        map(self.iter(), out);
    }
}

impl<T: Ord, A: Allocator> DebugForm for SortedSet<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        set(self.as_slice().iter(), out);
    }
}

impl<T, A: Allocator> DebugForm for BTreeSet<T, A> {
    fn debug_form(&self, out: &mut dyn Put) {
        set(self.iter(), out);
    }
}

/// `PhantomData<u32>`, as `{:?}` writes it.
impl<T: ?Sized> DebugForm for PhantomData<T> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.str("PhantomData<").str(core::any::type_name::<T>()).put_byte(b'>');
    }
}

/// The address, as `{:?}` writes it.
impl<T: ?Sized> DebugForm for NonNull<T> {
    fn debug_form(&self, out: &mut dyn Put) {
        out.str("0x").hex(self.as_ptr().addr());
    }
}

/// Stops the build: `name` has no debug form blaze-text can read.
#[rustc_comptime]
fn refuse(name: &'static str, why: &'static str) -> ! {
    panic!("{}", join(&["blaze-text: no debug form for `", name, "`: it ", why, "; implement `blaze_text::DebugForm` for it"]))
}

/// The pieces as one string, at compile time.
#[rustc_comptime]
fn join(pieces: &[&str]) -> &'static str {
    let mut len = 0;
    let mut i = 0;
    while i < pieces.len() {
        len += pieces[i].len();
        i += 1;
    }
    let at = allocate::<u8>(len);
    let mut filled = 0;
    let mut i = 0;
    while i < pieces.len() {
        let bytes = pieces[i].as_bytes();
        let mut j = 0;
        while j < bytes.len() {
            // SAFETY: `filled < len`, the sum of the pieces' lengths.
            unsafe { at.add(filled).write(bytes[j]) };
            filled += 1;
            j += 1;
        }
        i += 1;
    }
    match core::str::from_utf8(keep(at, len)) {
        Ok(text) => text,
        Err(_) => "blaze-text: no debug form",
    }
}
