//! What each call of the crate expands to has to do, as its macro's documentation says. Built
//! by blaze, these hold blaze's expansions to the macros' own promises.

mod format;

use std::borrow::Cow;
use std::error::Error as _;
use std::marker::PhantomData;

use format::{In, from, to_text};

use super::*;

#[test]
fn serde_writes_fields_in_order() {
    let plain = Plain { a: 1, b: "x".to_owned() };
    assert_eq!(to_text(&plain).unwrap(), r#"{"a":1,"b":"x"}"#);
    let read: Plain = from(In::Map(vec![("a", In::U(2)), ("b", In::S("y"))])).unwrap();
    assert_eq!((read.a, read.b.as_str()), (2, "y"));
}

#[test]
fn serde_renames_skips_and_refuses_unknown_fields() {
    let renamed = Renamed { user_id: 7, cache: Some("kept out".to_owned()), display_note: None };
    assert_eq!(to_text(&renamed).unwrap(), r#"{"id":7}"#);
    let noted = Renamed { display_note: Some("n".to_owned()), ..renamed };
    assert_eq!(to_text(&noted).unwrap(), r#"{"id":7,"displayNote":"n"}"#);
    let read: Renamed = from(In::Map(vec![("id", In::U(9)), ("displayNote", In::S("m"))])).unwrap();
    assert_eq!((read.user_id, read.cache, read.display_note.as_deref()), (9, None, Some("m")));
    let defaulted: Renamed = from(In::Map(vec![("id", In::U(1))])).unwrap();
    assert_eq!(defaulted.display_note, None);
    assert!(from::<Renamed>(In::Map(vec![("id", In::U(1)), ("userId", In::U(1))])).is_err());
}

#[test]
fn serde_flattens() {
    let outer = Outer { name: "o".to_owned(), inner: Plain { a: 3, b: "z".to_owned() } };
    assert_eq!(to_text(&outer).unwrap(), r#"{"name":"o","a":3,"b":"z"}"#);
    let read: Outer = from(In::Map(vec![("name", In::S("p")), ("a", In::U(4)), ("b", In::S("w"))])).unwrap();
    assert_eq!((read.name.as_str(), read.inner.a, read.inner.b.as_str()), ("p", 4, "w"));
}

#[test]
fn serde_keeps_generics_and_skips_phantoms() {
    let wrapper: Wrapper<u8, 2> = Wrapper { value: 1, items: vec![2, 3], marker: PhantomData };
    assert_eq!(to_text(&wrapper).unwrap(), r#"{"value":1,"items":[2,3]}"#);
    let read: Wrapper<u8, 2> = from(In::Map(vec![("value", In::U(5)), ("items", In::Seq(vec![In::U(6)]))])).unwrap();
    assert_eq!((read.value, read.items), (5, vec![6]));
}

#[test]
fn serde_borrows() {
    let read: Borrowed<'_, '_> =
        from(In::Map(vec![("text", In::S("t")), ("more", In::S("m")), ("n", In::I(-1))])).unwrap();
    assert_eq!((read.text, read.n), ("t", -1));
    assert!(matches!(read.more, Cow::Borrowed("m")), "`#[serde(borrow)]` borrows the Cow");
    assert_eq!(to_text(&read).unwrap(), r#"{"text":"t","more":"m","n":-1}"#);
}

#[test]
fn serde_tags_variants_inside() {
    assert_eq!(to_text(&Shape::Circle { r: 1.5 }).unwrap(), r#"{"kind":"circle","r":1.5}"#);
    assert_eq!(to_text(&Shape::Square { side: 2 }).unwrap(), r#"{"kind":"box","side":2}"#);
    assert_eq!(to_text(&Shape::Point).unwrap(), r#"{"kind":"point"}"#);
    let square: Shape = from(In::Map(vec![("kind", In::S("box")), ("side", In::U(4))])).unwrap();
    assert!(matches!(square, Shape::Square { side: 4 }));
    let point: Shape = from(In::Map(vec![("kind", In::S("point"))])).unwrap();
    assert!(matches!(point, Shape::Point));
    assert!(from::<Shape>(In::Map(vec![("kind", In::S("square")), ("side", In::U(4))])).is_err());
}

#[test]
fn serde_skips_a_tuple_field() {
    assert_eq!(to_text(&Pair(1, 2)).unwrap(), "[1]");
    let read: Pair = from(In::Seq(vec![In::U(8)])).unwrap();
    assert_eq!((read.0, read.1), (8, 0));
}

#[test]
fn thiserror_displays_and_chains() {
    let bad = Failure::Bad { value: -3, at: 4 };
    assert_eq!(bad.to_string(), "bad value -3 at 4");
    assert!(bad.source().is_none());
    let io = Failure::from(std::io::Error::other("disk"));
    assert_eq!(io.to_string(), "io failed: disk");
    assert_eq!(io.source().map(ToString::to_string).as_deref(), Some("disk"));
    let wrapped = Failure::Wrapped { cause: std::fmt::Error };
    assert_eq!(wrapped.to_string(), "wrapped");
    assert!(wrapped.source().is_some());
    assert_eq!(ParseError { line: 3 }.to_string(), "parse error on line 3");
    let parsed = "x".parse::<u8>().unwrap_err();
    let opaque = Opaque::from(parsed.clone());
    assert_eq!(opaque.to_string(), parsed.to_string(), "`#[error(transparent)]` is its source's display");
    assert!(opaque.source().is_none(), "and its source's source");
    let generic = Generic { name: "g", source: ParseError { line: 1 } };
    assert_eq!(generic.to_string(), "g of parse error on line 1");
    assert_eq!(generic.source().map(ToString::to_string).as_deref(), Some("parse error on line 1"));
}

#[test]
fn tokio_main_runs_its_body() {
    tokio_main();
}

#[test]
fn tokio_declares_selects_output() {
    let outs: [select::Out<u8, u16, u32>; 4] = [select::Out::_0(1), select::Out::_1(2), select::Out::_2(3), select::Out::Disabled];
    assert_eq!(outs.len(), 4);
    let mask: select::Mask = u8::MAX;
    assert_eq!(mask, 255);
}

#[test]
fn the_corpus_macros_do_what_they_say() {
    assert_eq!(Described::FIELDS, ["a", "pairs", "call"]);
    assert_eq!((labelled(), labelled_again(2)), (1, 3));
    assert_eq!((answer(), greeting()), (42, "hi"));
    assert_eq!(DATA, include_str!("../data.txt"));
    assert_eq!(PACKAGE, "corpus-calls");
    assert!(PACKAGE_UNSET, "CORPUS_UNSET is unset where the crate is built");
    assert_eq!((FIRST, SECOND), (0, 1), "`counter!` counts its calls in the order rustc expands them");
}
