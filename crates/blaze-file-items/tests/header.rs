//! `impl_header` on the headers `core` holds (2,393 of its impls were skipped by a text reader
//! that knew neither `const impl` nor `impl const`), and the outline of core's nightly items
//! (`final fn` in a trait, a const closure in a body) without a syntax error.

use blaze_file_items::{HeaderError, impl_header, outline};
use blaze_surface::{Edition, ImplFlags};

fn read(text: &str) -> (ImplFlags, Vec<&str>, Option<&str>, &str) {
    match impl_header(text, Edition::E2024, blaze_own::testing::heap().alloc()) {
        Ok(h) => (h.flags, h.params.iter().copied().collect(), h.trait_, h.self_ty),
        Err(error) => panic!("`{text}` does not read: {error}"),
    }
}

#[test]
fn const_impl_headers_read_as_const() {
    let both = [
        "const impl<T: ?Sized> Borrow<T> for T",
        "impl<T: ?Sized> const Borrow<T> for T",
    ];
    for text in both {
        let (flags, params, trait_, self_ty) = read(text);
        assert_eq!(flags, ImplFlags::CONST, "{text}");
        assert_eq!(params, ["T"], "{text}");
        assert_eq!(trait_, Some("Borrow<T>"), "{text}");
        assert_eq!(self_ty, "T", "{text}");
    }
    let (flags, ..) = read("unsafe const impl Send for S");
    assert_eq!(flags, ImplFlags::UNSAFE.union(ImplFlags::CONST));
}

#[test]
fn headers_read_by_tokens() {
    // `for` and `where` on their own lines (core/src/slice/cmp.rs:349).
    let (_, params, trait_, self_ty) =
        read("impl<T, U, const N: usize>\n    PartialEq<[U; N]>\n    for [T]\nwhere\n    T: PartialEq<U>,");
    assert_eq!(params, ["T", "U", "N"]);
    assert_eq!(trait_, Some("PartialEq<[U; N]>"));
    assert_eq!(self_ty, "[T]");
    let (flags, params, trait_, self_ty) = read("impl<'a, F: Fn() -> u8> !Send for &'a F");
    assert_eq!(flags, ImplFlags::NEGATIVE);
    assert_eq!(params, ["F"]);
    assert_eq!((trait_, self_ty), (Some("Send"), "&'a F"));
    let (flags, params, trait_, self_ty) = read("default unsafe impl<T> Trait for Vec<Vec<T>>");
    assert_eq!(flags, ImplFlags::DEFAULT.union(ImplFlags::UNSAFE));
    assert_eq!((params.len(), trait_, self_ty), (1, Some("Trait"), "Vec<Vec<T>>"));
    let (_, _, trait_, self_ty) = read("impl dyn Error + 'static");
    assert_eq!((trait_, self_ty), (None, "dyn Error + 'static"));
    let (_, _, trait_, self_ty) = read("impl<T> Fn<()> for for<'a> fn(&'a T)");
    assert_eq!((trait_, self_ty), (Some("Fn<()>"), "for<'a> fn(&'a T)"));
    let refused = |text| impl_header(text, Edition::E2024, blaze_own::testing::heap().alloc()).map(|_| ());
    assert_eq!(refused("struct S"), Err(HeaderError::NoImpl));
    assert_eq!(refused("impl<T"), Err(HeaderError::Unclosed));
    assert_eq!(refused("impl Trait for"), Err(HeaderError::NoSelfType));
}

#[test]
fn cores_nightly_items_outline_without_errors() {
    let text = "pub trait Share: Clone {\n    /// Doc.\n    final fn share(&self) -> Self { self.clone() }\n}\n\
                fn f<T>(mut f: impl FnMut(T) -> bool) -> impl FnMut((), T) {\n    const move |(), x| { f(x); }\n}\n\
                const impl<T: ?Sized> Borrow<T> for T {}\n";
    let outline = outline(text, Edition::E2024, blaze_own::testing::heap().alloc());
    let errors: Vec<String> = outline.errors().iter().map(|it| outline.message(it).to_string()).collect();
    assert_eq!(errors, Vec::<String>::new());
}
