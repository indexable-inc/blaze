//! An item text that SPELLS `$crate` (a body's lowering copies an item a macro call in the body
//! wrote out of the expansion's words, and those spell it: blaze-ladder front2 locals.rs `keep`)
//! is lowered with `$crate` one word, a path root (lib.rs `spelled_words`), as an item macro's
//! words are (`lower_signature_words`).
//!
//! The stop it fixes: std's `thread_local_inner!` writes, inside a closure of a const block of
//! `OUTPUT_CAPTURE`'s initializer, `#[thread_local] static __RUST_STD_INTERNAL_VAL:
//! $crate::thread::local_impl::EagerStorage<..> = $crate::thread::local_impl::EagerStorage::new
//! (..);`. Lexed by the parser's lexer, `$` stopped the type, the `=` fell in an error node, and
//! the static had no initializer (`sig::HAS_BODY` unset): "a fn or static declared in a body
//! with no body" (g204119, std hello world).
//!
//! The control (rule 23): without the fold (`spelled_words` returning `None`), the first test's
//! `$crate` static has no `HAS_BODY` and its type is no path ending at `B`; the second test, a
//! text with no `$crate`, passes either way.
//!
//! Prints "dollar crate text: static HAS_BODY, type `$crate::a::B`".

use blaze_lower::machine::sig;
use blaze_lower::{Cfg, CfgAnswer, Edition, TypeId, head, lower_signature};

struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::No
    }
}

/// `text`'s signature: whether it has a body, and its declared type's text.
fn signature(text: &str) -> (bool, String) {
    let alloc = blaze_own::testing::heap().alloc();
    let l = match lower_signature(text, Edition::E2024, &mut NoCfg, alloc) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let v = l.view();
    let has_body = v.head[head::FLAGS] & sig::HAS_BODY != 0;
    let ty = match v.head_opt(head::TY) {
        Some(t) => {
            let r = v.type_range(TypeId(t)).unwrap_or_else(|e| panic!("{text}: the type's range: {e:?}"));
            v.text_at(r).unwrap_or_else(|e| panic!("{text}: the type's text: {e:?}")).to_string()
        }
        None => String::from("<no type>"),
    };
    (has_body, ty)
}

/// A static as a body expansion's copy spells it (with `#[thread_local]`, spaced as the copy
/// is): its initializer is its own (`HAS_BODY`), its type the whole `$crate` path.
#[test]
fn a_static_spelling_dollar_crate_has_its_initializer() {
    let text = "#[thread_local] static V : $crate::a::B < u8 > = $crate::a::B::new (C) ;";
    let (has_body, ty) = signature(text);
    assert!(has_body, "the static's `=` is its own: it has an initializer");
    assert_eq!(ty, "$crate::a::B < u8 >", "the type is the whole `$crate` path");
    println!("dollar crate text: static HAS_BODY, type `{ty}`");
}

/// The same static written with `crate` (a module-level rendering's spelling): read as written.
#[test]
fn a_static_spelling_crate_is_read_as_written() {
    let (has_body, ty) = signature("static V: crate::a::B = crate::a::B::new(C);");
    assert!(has_body, "the static has an initializer");
    assert_eq!(ty, "crate::a::B");
}

/// A fn whose parameter and return types spell `$crate`: a head with no body is `HAS_BODY`
/// clear, one with a block set; `$crate` inside the body is never read here.
#[test]
fn a_fn_spelling_dollar_crate_keeps_its_body_flag() {
    let (with_body, _) = signature("fn get (x : $crate::a::B) -> $crate::a::B { $crate::a::id (x) }");
    assert!(with_body, "a fn with a block has a body");
    let (without, _) = signature("fn get (x : $crate::a::B) -> $crate::a::B ;");
    assert!(!without, "a fn with `;` has none");
}
