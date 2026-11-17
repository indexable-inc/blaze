//! A codec written once by looping over fields (docs/v2/plan/design/comptime.md, "Building a
//! struct"): `field_names` and `encode` over a struct of three fields of different types, and
//! `decode` through `build`, the library that makes a struct field by field from a
//! `FieldSource`, run in the evaluator. What `bstd::data` is built on.

/// `library/core/src/{field,reflect}.rs` reduced to what the codec runs (the field
/// representing type, `Field::{NAME, INDEX}` from their intrinsics, `field_offset`,
/// `fields_of`), the evaluator's raw-pointer intrinsics, and a `write` to stdout.
const REFLECT_CODEC: &str = r#"
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type;
}
#[lang = "field_representing_type"]
/// Zero-sized, as core's (`library/core/src/field.rs`): a loop's binding is its one value.
pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {
    _phantom: core::marker::PhantomData<fn(T) -> T>,
}
pub struct TypeId(u128);
#[rustc_intrinsic]
pub const fn type_id<T: ?Sized>() -> TypeId;
#[rustc_intrinsic]
pub const fn field_representing_type_name(id: TypeId) -> &'static str;
#[rustc_intrinsic]
pub const fn blaze_field_index<F: FieldOf>() -> usize;
#[rustc_intrinsic]
pub const fn field_offset<F: FieldOf>() -> usize;
#[rustc_intrinsic]
pub const unsafe fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;
#[rustc_intrinsic]
pub const unsafe fn write_via_move<T>(ptr: *mut T, value: T);
#[rustc_intrinsic]
pub const fn forget<T>(value: T);
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {
    const NAME: &'static str = field_representing_type_name(type_id::<Self>());
    const INDEX: usize = blaze_field_index::<Self>();
    fn name(self) -> &'static str { Self::NAME }
    fn index(self) -> usize { Self::INDEX }
}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_struct"]
pub unsafe trait Struct {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub const fn fields_of<T: Struct>() -> Fields<T> { Fields(0usize as *const T, ()) }

extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}
fn put(s: &str) {
    let b = s.as_bytes();
    unsafe { write(1, b as *const [u8] as *const u8, b.len()); }
}
fn digit(n: u8) {
    let d = [b'0' + n];
    unsafe { write(1, &d[0], 1); }
}
"#;

/// `build` and `FieldSource` as the design writes them (docs/v2/plan/design/comptime.md,
/// "Building a struct"), with the fixture's own `MaybeUninit`.
const BUILD: &str = r#"
/// What `build` takes each field's value from: asked once per field, in declaration order.
pub trait FieldSource<Base: ?Sized> {
    type Error;
    fn take<F: Field<Base>>(&mut self, field: F) -> Result<<F as FieldOf>::Type, Self::Error>;
}

pub union MaybeUninit<T> {
    uninit: (),
    value: core::mem::ManuallyDrop<T>,
}

/// The fields `build` wrote so far, dropped if it stops before the last.
struct Written<T> {
    out: *mut T,
    count: usize,
    drop: unsafe fn(*mut T, usize),
}
impl<T> Drop for Written<T> {
    fn drop(&mut self) {
        unsafe { (self.drop)(self.out, self.count) }
    }
}

/// Where the field `F` of the `T` at `base` is.
fn field_ptr<T, F: Field<T>>(base: *mut T, _: F) -> *mut <F as FieldOf>::Type {
    unsafe {
        arith_offset(base as *const u8, field_offset::<F>() as isize)
            as *mut <F as FieldOf>::Type
    }
}

/// Drops the first `count` fields, in declaration order, of the `T` at `out`.
unsafe fn drop_written<T: Struct>(out: *mut T, count: usize) {
    for const f in fields_of::<T>() {
        if f.index() < count {
            unsafe { core::ptr::drop_in_place(field_ptr(out, f)) }
        }
    }
}

/// A `T` whose every field, in declaration order, is what `source` gives for it.
pub fn build<T: Struct, S: FieldSource<T>>(mut source: S) -> Result<T, S::Error> {
    let mut out = MaybeUninit::<T> { uninit: () };
    let base = &mut out as *mut MaybeUninit<T> as *mut T;
    let mut written = Written { out: base, count: 0, drop: drop_written::<T> };
    for const f in fields_of::<T>() {
        match source.take(f) {
            Ok(value) => unsafe { write_via_move(field_ptr(base, f), value) },
            Err(error) => return Err(error),
        }
        written.count += 1;
    }
    forget(written);
    Ok(unsafe { core::ptr::read(base) })
}
"#;

const FLAGS: &str = "//- minicore: sized, copy, clone, slice, index, coerce_unsized, str, \
                     phantom_data, drop, manually_drop, result\n\
                     #![feature(blaze_comptime, lang_items)]\n";

/// The two target functions of phase 2's codec, over a struct of a `u8`, a string and a
/// `bool`: the names in declaration order, then each field's name and encoding.
#[test]
fn field_names_and_encode_loop_over_the_fields_in_declaration_order() {
    super::eval::check_pass_and_stdio(
        &format!(
            "{FLAGS}{REFLECT_CODEC}\n{}",
            r#"
/// A stand-in for `alloc::vec::Vec` that prints what is pushed, which is what the test reads.
pub struct Vec<T>(core::marker::PhantomData<T>);
impl<T> Vec<T> {
    pub fn new() -> Vec<T> { Vec(core::marker::PhantomData) }
}
pub trait Shown { fn shown(&self); }
impl<T: Shown> Vec<T> {
    pub fn push(&mut self, value: T) { value.shown(); }
}
impl Shown for u8 {
    fn shown(&self) {
        let d = [b'0' + *self / 100, b'0' + *self / 10 % 10, b'0' + *self % 10, b' '];
        unsafe { write(1, &d[0], 4); }
    }
}
impl Shown for &'static str {
    fn shown(&self) { put(self); put(";"); }
}

pub trait Encode { fn encode(&self, out: &mut Vec<u8>); }
impl Encode for u8 {
    fn encode(&self, out: &mut Vec<u8>) { out.push(*self); }
}
impl Encode for bool {
    fn encode(&self, out: &mut Vec<u8>) { out.push(*self as u8); }
}
pub struct Text(pub &'static str);
impl Encode for Text {
    fn encode(&self, out: &mut Vec<u8>) { put_str(out, self.0); }
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.push(bytes.len() as u8);
    let mut at = 0;
    while at < bytes.len() {
        out.push(bytes[at]);
        at += 1;
    }
}

fn field_names<T: Struct>() -> Vec<&'static str> {
    let mut v = Vec::new();
    for const f in fields_of::<T>() { v.push(f.name()); }
    v
}
fn encode<T: Struct>(value: &T, out: &mut Vec<u8>)
where for<F: Field<T>> F::Type: Encode
{
    for const f in fields_of::<T>() { put_str(out, f.name()); value.[f].encode(out); }
}

pub struct S { pub a: u8, pub b: Text, pub c: bool }

fn main() {
    let _names = field_names::<S>();
    put("\n");
    let mut out = Vec::new();
    encode(&S { a: 7, b: Text("hi"), c: true }, &mut out);
    put("\n");
}
"#
        ),
        // `a;b;c;`, then per field its name (length, bytes) and value: `a` 7, `b` "hi", `c` true.
        "a;b;c;\n001 097 007 001 098 002 104 105 001 099 001 \n",
        "",
    );
}

/// `decode` through `build`: each field is decoded in declaration order by a `FieldSource`
/// whose generic `take` uses the caller's `for<F: Field<T>> F::Type: Decode`. When the input
/// runs out at the third field, the two fields written are dropped, in order, and no value is
/// made.
#[test]
fn decode_builds_a_struct_field_by_field_and_drops_what_it_wrote_on_error() {
    super::eval::check_pass_and_stdio(
        &format!(
            "{FLAGS}{REFLECT_CODEC}\n{BUILD}\n{}",
            r#"
pub struct Short;
pub struct Input { pub bytes: [u8; 4], pub len: usize, pub at: usize }
impl Input {
    fn byte(&mut self) -> Result<u8, Short> {
        if self.at < self.len {
            let byte = self.bytes[self.at];
            self.at += 1;
            Ok(byte)
        } else {
            Err(Short)
        }
    }
}
pub trait Decode: Sized { fn decode(input: &mut Input) -> Result<Self, Short>; }
impl Decode for u8 {
    fn decode(input: &mut Input) -> Result<u8, Short> { input.byte() }
}
impl Decode for bool {
    fn decode(input: &mut Input) -> Result<bool, Short> {
        match input.byte() {
            Ok(byte) => Ok(byte != 0),
            Err(short) => Err(short),
        }
    }
}
impl Decode for u16 {
    fn decode(input: &mut Input) -> Result<u16, Short> {
        let low = match input.byte() {
            Ok(it) => it,
            Err(short) => return Err(short),
        };
        let high = match input.byte() {
            Ok(it) => it,
            Err(short) => return Err(short),
        };
        Ok(low as u16 + (high as u16) * 256)
    }
}
pub struct Loud(pub u8);
impl Drop for Loud {
    fn drop(&mut self) { put("~"); digit(self.0); }
}
impl Decode for Loud {
    fn decode(input: &mut Input) -> Result<Loud, Short> {
        match input.byte() {
            Ok(byte) => Ok(Loud(byte)),
            Err(short) => Err(short),
        }
    }
}

pub struct Decoder<'a>(pub &'a mut Input);
impl<'a, T> FieldSource<T> for Decoder<'a> where for<F: Field<T>> F::Type: Decode {
    type Error = Short;
    fn take<F: Field<T>>(&mut self, _: F) -> Result<F::Type, Short> {
        Decode::decode(&mut *self.0)
    }
}

fn decode<T: Struct>(input: &mut Input) -> Result<T, Short>
where for<F: Field<T>> F::Type: Decode
{
    build(Decoder(input))
}

pub struct D { pub a: u8, pub b: bool, pub c: u16 }
pub struct P { pub a: Loud, pub b: Loud, pub c: u16 }

fn main() {
    let mut input = Input { bytes: [7, 1, 3, 0], len: 4, at: 0 };
    match decode::<D>(&mut input) {
        Ok(d) => {
            digit(d.a);
            put(if d.b { " true " } else { " false " });
            digit(d.c as u8);
        }
        Err(_) => put("short"),
    }
    put("\n");
    let mut input = Input { bytes: [1, 2, 0, 0], len: 2, at: 0 };
    match decode::<P>(&mut input) {
        Ok(_) => put("decoded"),
        Err(_) => put("short"),
    }
    put("\n");
}
"#
        ),
        "7 true 3\n~1~2short\n",
        "",
    );
}
