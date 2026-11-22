//! [`lower_type_text`]'s tests: a type from its text equals, node for node, the same text's
//! type in a fixture signature; trailing tokens and an empty text are refused by name.

use std::format;
use std::string::String;

use blaze_own::Alloc;

use crate::form::*;
use crate::{Cfg, CfgAnswer, Edition, Lowered, TypeTextRefused, head, lower_signature, lower_type_text};

/// No `cfg` atom is asked by a type in these tests.
struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, name: &str, _value: Option<&str>) -> CfgAnswer {
        panic!("a type text asked the cfg atom `{name}`")
    }
}

/// The forms the pinned core writes as a pattern type's base (`u32`, `*const T`, `*mut T`,
/// `T`), then a path with generic arguments, references, a tuple, an array with a literal
/// length, a slice, and a type that is itself a macro call.
const TYPES: [&str; 13] = [
    "u32",
    "*const T",
    "*mut T",
    "T",
    "core::option::Option<u8>",
    "&'a mut [u8]",
    "&T",
    "(u8, *const T, ())",
    "[u8; 4]",
    "Vec<Vec<u8>>",
    "fn(u8) -> u16",
    "dyn Fn(u8) + Send",
    "foo!(u8, T)",
];

fn dump_type_of(l: &Lowered<'_, '_>, t: TypeId) -> String {
    let mut out = String::new();
    crate::dump::dump_type(&l.view(), t, Alloc::global(), &mut out).expect("a String takes every write");
    out
}

/// The main test: for each text, the lone type's tree is the signature's parameter type's
/// tree, node for node through the one type printer (every node, path segment, generic
/// argument, lifetime, length and macro row is printed), and both cover the same text. The
/// two lowerings share the machine, so they can never drift; this shows they do not.
#[test]
fn a_type_text_equals_the_same_text_in_a_signature() {
    for text in TYPES {
        let lone = match lower_type_text(text, Edition::E2024, &mut NoCfg, Alloc::global()) {
            Ok(l) => l,
            Err(why) => panic!("{text}: {why}"),
        };
        let root = lone.view().type_root().unwrap_or_else(|| panic!("{text}: no root type"));
        // The macro call is the one node counted unhandled (kept, not expanded), as in a signature.
        let unhandled = lone.view().unhandled().count();
        assert_eq!(unhandled, usize::from(text.contains('!')), "{text}: unhandled nodes");

        let fixture = format!("fn f(_: {text}) {{}}");
        let sig = match lower_signature(&fixture, Edition::E2024, &mut NoCfg, Alloc::global()) {
            Ok(l) => l,
            Err(need) => panic!("{fixture}: needs {:?}", need.atoms),
        };
        let view = sig.view();
        let params = view.rows(view.list(head::PARAMS)).expect("the fixture's parameter rows");
        assert_eq!(params.len(), 1, "{fixture}: one parameter");
        let param = Param::decode(params[0]).expect("a parameter row");
        let sig_ty = param.ty.unwrap_or_else(|| panic!("{fixture}: the parameter has no type"));

        let a = dump_type_of(&lone, root);
        let b = dump_type_of(&sig, sig_ty);
        assert_eq!(a, b, "{text}: the lone type and the signature's differ");
        assert!(!a.contains("(error)"), "{text}: lowered to an error node: {a}");

        let lone_text = lone.view().text_at(lone.view().type_range(root).expect("the root's range")).expect("its text");
        let sig_text = view.text_at(view.type_range(sig_ty).expect("the parameter type's range")).expect("its text");
        assert_eq!(lone_text, text, "{text}: the root covers the whole text");
        assert_eq!(sig_text, text, "{text}: the signature's type covers the same text");
    }
}

/// The root is the form written, not a wrapper: a primitive and a parameter are paths, a raw
/// pointer is `Ptr` with its mutability, a macro call is the unexpanded type-macro row.
#[test]
fn the_root_is_the_written_form() {
    let cases: [(&str, fn(TypeRef) -> bool); 5] = [
        ("u32", |t| matches!(t, TypeRef::Path(_))),
        ("T", |t| matches!(t, TypeRef::Path(_))),
        ("*const T", |t| matches!(t, TypeRef::Ptr { mutbl: Mutbl::Not, .. })),
        ("*mut T", |t| matches!(t, TypeRef::Ptr { mutbl: Mutbl::Mut, .. })),
        ("  foo!(u8)  ", |t| matches!(t, TypeRef::Macro(_))),
    ];
    for (text, is) in cases {
        let lone = match lower_type_text(text, Edition::E2024, &mut NoCfg, Alloc::global()) {
            Ok(l) => l,
            Err(why) => panic!("{text}: {why}"),
        };
        let view = lone.view();
        let root = view.type_root().unwrap_or_else(|| panic!("{text}: no root type"));
        let t = view.ty(root).expect("the root's row");
        assert!(is(t), "{text}: root {t:?}");
    }
}

/// Tokens after the type are refused, naming the first one and its byte offset.
#[test]
fn trailing_tokens_are_refused_at_their_offset() {
    for (text, at, token) in [("u32 x", 4, "x"), ("*const T, u8", 8, ","), ("Option<u8> >", 11, ">")] {
        match lower_type_text(text, Edition::E2024, &mut NoCfg, Alloc::global()) {
            Err(TypeTextRefused::Trailing { offset, token: t }) => {
                assert_eq!((offset, t), (at, token), "{text}");
            }
            Err(other) => panic!("{text}: refused as {other}, not as trailing"),
            Ok(_) => panic!("{text}: lowered"),
        }
    }
}

/// A text with no token is refused as empty, never lowered to an error type.
#[test]
fn an_empty_text_is_refused() {
    for text in ["", "   ", "/* only a comment */"] {
        match lower_type_text(text, Edition::E2024, &mut NoCfg, Alloc::global()) {
            Err(TypeTextRefused::Empty) => {}
            Err(other) => panic!("{text:?}: refused as {other}, not as empty"),
            Ok(_) => panic!("{text:?}: lowered"),
        }
    }
}
