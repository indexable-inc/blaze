//! The `Hash` half of phase 2's parity test (docs/v2/plan/design/comptime.md, "Tests phase 2
//! must land with"): `Hash` written once with `for const` feeds a hasher the same calls, in the
//! same order, as `#[derive(Hash)]` on the struct. The `Debug` half against a hand-written impl
//! is `comptime::a_for_const_debug_prints_what_the_hand_written_one_does`.
//!
//! minicore's `Hasher` has no `write_*` methods, so the record is kept by the leaves instead:
//! each leaf type's `Hash` prints its value, and the two outputs are compared. A field whose
//! type is itself `#[derive(Hash)]` shows the reflected form calls the field's own impl,
//! as the derive does, rather than reaching into it.

/// `library/core/src/reflect.rs` and `core::field` reduced to what the expansion of a
/// `for const` over a struct's fields needs, and a `write` to stdout.
const REFLECT_HASH: &str = r#"
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "field_representing_type"]
/// Zero-sized, as core's (`library/core/src/field.rs`): a loop's binding is its one value.
pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {
    _phantom: core::marker::PhantomData<fn(T) -> T>,
}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_struct"]
pub unsafe trait Struct {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub const fn fields_of<T>() -> Fields<T> { Fields(0usize as *const T, ()) }

extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}
fn put(s: &str) {
    let b = s.as_bytes();
    unsafe { write(1, b as *const [u8] as *const u8, b.len()); }
}
"#;

#[test]
fn a_for_const_hash_feeds_the_hasher_what_derive_hash_does() {
    // `#[derive(Hash)]` is run, so the derives are expanded.
    super::eval::check_pass_and_stdio_derived(
        &format!(
            "//- minicore: sized, copy, clone, hash, derive, slice, index, coerce_unsized, str, phantom_data\n\
             #![feature(blaze_comptime, lang_items)]\n{REFLECT_HASH}\n{}",
            r#"
use core::hash::{Hash, Hasher};

/// Written once for every struct whose fields are all `Hash`.
fn hash_fields<T, H: Hasher>(t: &T, state: &mut H) where for<F: Field<T>> F::Type: Hash {
    for const f in fields_of::<T>() {
        Hash::hash(&t.[f], state);
    }
}

pub struct Byte(pub u8);
impl Hash for Byte {
    fn hash<H: Hasher>(&self, _: &mut H) {
        let digit = [b'0' + self.0];
        unsafe { write(1, &digit[0], 1); }
        put(";");
    }
}
pub struct Text(pub &'static str);
impl Hash for Text {
    fn hash<H: Hasher>(&self, _: &mut H) {
        put(self.0);
        put(";");
    }
}
pub struct Flag(pub bool);
impl Hash for Flag {
    fn hash<H: Hasher>(&self, _: &mut H) {
        put(if self.0 { "true;" } else { "false;" });
    }
}

#[derive(Hash)]
pub struct Pair { pub n: Byte, pub on: Flag }

#[derive(Hash)]
pub struct S { pub a: Byte, pub b: Text, pub c: Pair }

pub struct Recorder;
impl Hasher for Recorder {}

fn main() {
    let s = S { a: Byte(7), b: Text("hi"), c: Pair { n: Byte(3), on: Flag(true) } };
    hash_fields(&s, &mut Recorder);
    put("\n");
    Hash::hash(&s, &mut Recorder);
    put("\n");
}
"#
        ),
        "7;hi;3;true;\n7;hi;3;true;\n",
        "",
    );
}
