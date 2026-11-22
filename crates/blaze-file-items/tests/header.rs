//! `impl_header` on the headers `core` holds (2,393 of its impls were skipped by a text reader
//! that knew neither `const impl` nor `impl const`), and the outline of core's nightly items
//! (`final fn` in a trait, a const closure in a body) without a syntax error.

use blaze_file_items::{GenericsError, HeaderError, declared_lifetimes, impl_header, outline};
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

/// An item's declared lifetime count (D155), the one fact the region walk counts an ADT's,
/// trait's or alias's slots by. Each case's planted control is the case beside it that differs
/// in the one token the rule is about: a lifetime at a parameter's start counts, one anywhere
/// else (a default, a bound, a where clause, a `for<..>`) does not.
#[test]
fn an_item_counts_the_lifetimes_its_generics_declare() {
    let n = |text: &str| declared_lifetimes(text, Edition::E2024);
    // No generics.
    assert_eq!(n("pub struct S;"), Ok(0));
    assert_eq!(n("enum E { A, B }"), Ok(0));
    // Lifetimes only, with a bound between them.
    assert_eq!(n("struct S<'a, 'b: 'a>(&'a &'b u8);"), Ok(2));
    // Mixed order: a type, a lifetime, a const, a lifetime.
    assert_eq!(n("pub(crate) struct S<T, 'a, const N: usize, 'b> { t: &'a [T; N], u: &'b u8 }"), Ok(2));
    // A default holding a lifetime is no parameter; CONTROL: the same text with a lifetime
    // parameter counts it.
    assert_eq!(n("pub struct V<T, A = Foo<'static>> { t: T, a: A }"), Ok(0));
    assert_eq!(n("pub struct V<'x, T, A = Foo<'static>> { t: &'x T, a: A }"), Ok(1));
    // A bound and a where clause name lifetimes; only the parameter counts.
    assert_eq!(n("trait Tr<'a, T: 'a> where for<'x> T: Fn(&'x u8) -> &'x u8 {}"), Ok(1));
    // A `const` generic with a defaulted block; a fn pointer default with an arrow.
    assert_eq!(n("struct S<const N: usize = { 1 + 2 }, F = fn(&u8) -> &u8>(F);"), Ok(0));
    // A raw identifier as the name.
    assert_eq!(n("struct r#type<'a>(&'a u8);"), Ok(1));
    // Attributes and the keyword's place: `#[derive(..)]`, `pub(in crate::m)`, `union`.
    assert_eq!(n("#[derive(Clone)]\n#[repr(C)]\npub(in crate::m) union U<'a> { a: &'a u8 }"), Ok(1));
    // A type alias, a trait alias, a GAT (an associated type's own generics).
    assert_eq!(n("pub type R<'a, T> = Result<&'a T, E>;"), Ok(1));
    assert_eq!(n("trait A<'a> = B<'a> + C;"), Ok(1));
    assert_eq!(n("type Item<'a, 'b> where Self: 'a;"), Ok(2));
    // A macro-written item: an expansion's tokens, spaced as an expansion prints them.
    assert_eq!(n("# [automatically_derived] pub struct D < 'a , T > { v : & 'a T }"), Ok(1));
    // Refused by name: no item keyword, a bracket left open.
    assert_eq!(n("fn f<'a>(x: &'a u8) {}"), Err(GenericsError::NoKeyword));
    assert_eq!(n("struct S<'a"), Err(GenericsError::Unclosed));
}

/// An impl's parameter names past each parameter's own attributes (the one walk, header.rs
/// `Params`, as `declared_lifetimes` reads them). std's drop impls write `#[may_dangle]` before
/// a type or a lifetime parameter (library/alloc/src/vec/mod.rs: `unsafe impl<#[may_dangle] T,
/// A: Allocator> Drop for Vec<T, A>`). Before the one walk the `#` took the parameter's start
/// and its name was lost: `["A"]` for `Vec`'s, so `T` read as no parameter of the impl.
/// Controls (rule 23): the same headers without the attributes read the same names; several
/// attributes, and brackets and a lifetime inside one, change nothing.
#[test]
fn an_impl_s_parameters_read_past_their_own_attributes() {
    let (flags, params, trait_, self_ty) = read("unsafe impl<#[may_dangle] T, A: Allocator> Drop for Vec<T, A>");
    assert_eq!(flags, ImplFlags::UNSAFE);
    assert_eq!(params, ["T", "A"], "std's `Vec` drop impl");
    assert_eq!((trait_, self_ty), (Some("Drop"), "Vec<T, A>"));
    // CONTROL: no attribute, the same names.
    assert_eq!(read("unsafe impl<T, A: Allocator> Drop for Vec<T, A>").1, ["T", "A"]);
    // The pinned alloc's exact header (library/alloc/src/vec/mod.rs:4467, its signature: the
    // outline leaves the outer attributes out), `[const]` bounds and the line break included.
    let (flags, params, trait_, self_ty) =
        read("const unsafe impl<#[may_dangle] T: [const] Destruct, A: [const] Allocator + [const] Destruct> Drop\n    for Vec<T, A>");
    assert_eq!(flags, ImplFlags::UNSAFE.union(ImplFlags::CONST));
    assert_eq!(params, ["T", "A"], "the pinned `Vec` drop impl");
    assert_eq!((trait_, self_ty), (Some("Drop"), "Vec<T, A>"));
    // An attribute before a lifetime parameter: no name, and the next parameter's is kept.
    assert_eq!(read("unsafe impl<#[may_dangle] 'a, T> Drop for Ref<'a, T>").1, ["T"]);
    assert_eq!(read("unsafe impl<'a, T> Drop for Ref<'a, T>").1, ["T"]);
    // Several attributes, brackets and a lifetime inside one, and a const parameter after.
    assert_eq!(read("impl<#[a] #[b(c[d], 'x)] T, #[e] const N: usize> Tr for S<T, N>").1, ["T", "N"]);
    assert_eq!(read("impl<T, const N: usize> Tr for S<T, N>").1, ["T", "N"]);
}

/// The pinned std's lock headers, exactly as written (library/std/src/sync/poison/mutex.rs:269
/// and poison.rs:231, with their doc comments and attributes). `MutexGuard`'s `'a` carries its
/// own outer attribute, `#[rustc_assert_variance(covariant)] 'a`: it is still a parameter's
/// start, so the count is 1. Before the fix the walk took the `#` for the parameter's start and
/// counted 0, so `MutexGuard<'_, T>` in `Mutex::lock`'s `LockResult<MutexGuard<'_, T>>` was
/// refused as written with another lifetime count than declared (E0107, `LifetimeArgs`) on the
/// real two-crate fixture. Controls (rule 23): the same header without the attributes counts
/// the same; an attribute on a TYPE parameter adds no lifetime; a lifetime inside the
/// attribute's own tokens counts nothing; the alias, whose argument may hold a lifetime at its
/// use, declares none, and its `=` ends nothing the walk needs (it stops at the generics' `>`).
#[test]
fn a_parameter_s_own_attribute_keeps_its_start() {
    let n = |text: &str| declared_lifetimes(text, Edition::E2024);
    const MUTEX_GUARD: &str = "/// [`try_lock`]: Mutex::try_lock
#[must_use = \"if unused the Mutex will immediately unlock\"]
#[must_not_suspend = \"holding a MutexGuard across suspend \\
                      points can cause deadlocks, delays, \\
                      and cause Futures to not implement `Send`\"]
#[stable(feature = \"rust1\", since = \"1.0.0\")]
#[clippy::has_significant_drop]
#[cfg_attr(not(test), rustc_diagnostic_item = \"MutexGuard\")]
pub struct MutexGuard<
    #[rustc_assert_variance(covariant)] 'a,
    #[rustc_assert_variance(invariant)] T: ?Sized + 'a,
> {
    lock: &'a Mutex<T>,
    poison: poison::Guard,
}";
    assert_eq!(n(MUTEX_GUARD), Ok(1), "std's `MutexGuard`");
    // CONTROL: the attributes removed, the same count.
    assert_eq!(n("pub struct MutexGuard<'a, T: ?Sized + 'a> { lock: &'a Mutex<T>, poison: poison::Guard }"), Ok(1));
    // An attribute on a type parameter and one holding brackets and a lifetime of its own.
    assert_eq!(n("struct S<#[may_dangle] T, #[a(b[c], 'x)] 'y, #[d] 'z>(T, &'y u8, &'z u8);"), Ok(2));
    assert_eq!(n("struct S<#[a(b[c], 'x)] T>(T);"), Ok(0));
    const LOCK_RESULT: &str = "/// [`into_inner`]: PoisonError::into_inner
#[stable(feature = \"rust1\", since = \"1.0.0\")]
pub type LockResult<T> = Result<T, PoisonError<T>>;";
    assert_eq!(n(LOCK_RESULT), Ok(0), "std's `LockResult`");
    assert_eq!(n("#[stable(feature = \"rust1\", since = \"1.0.0\")]\npub struct PoisonError<T> {\n    data: T,\n    #[cfg(not(panic = \"unwind\"))]\n    _never: !,\n}"), Ok(0));
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
