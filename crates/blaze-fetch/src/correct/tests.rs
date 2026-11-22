//! Each rule of the correction has a fixture that fails without it.

use blaze_lex::Cpu;
use blaze_own::Alloc;
use blaze_semver::Version;

use super::{Refused, Row, TABLE, Table, Text};

/// syn 1.0.109 src/parse.rs:1160 as written: the method quickcheck_macros 1.0.0 calls
/// (`syn::Item::parse.parse(input)`), gated off on wasm32-wasi.
const SYN_PARSE_RS: &str = "pub trait Parser: Sized {\n    type Output;\n    #[cfg(all(\n        not(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\"))),\n        feature = \"proc-macro\"\n    ))]\n    fn parse(self, tokens: proc_macro::TokenStream) -> Result<Self::Output> {\n        self.parse2(proc_macro2::TokenStream::from(tokens))\n    }\n}\n";

/// syn 1.0.109 src/lib.rs:294 as written; it stands in for the other three files.
const SYN_SITE: &str = "#[cfg(all(\n    not(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\"))),\n    feature = \"proc-macro\"\n))]\nextern crate proc_macro;\n";

/// The checked-in syn row's digests (`shasum -a 256` of syn 1.0.109 as fetched).
const SYN_DIGESTS: [(&str, &str); 4] = [
    ("src/buffer.rs", "4d15f35273d485261be4f1a765ae03abc1daee9fc9dac5fb4f9b624d6b22cb58"),
    ("src/export.rs", "0cf50d70c32d5fddba8b1193032df62e560237c113df3e86ba26b565cc82838e"),
    ("src/lib.rs", "de155fed49dffb1756ca8b76d82209fe8bc80c4dda6684f669e98835147869a5"),
    ("src/parse.rs", "7b2f8caddf25a5734cbcdf7cbf043cbf9afbc07b484966cd59ddfcec9f970fb3"),
];

fn syn_file<'t>(path: &str) -> Option<&'t [u8]> {
    match path {
        "src/parse.rs" => Some(SYN_PARSE_RS.as_bytes()),
        "src/buffer.rs" | "src/export.rs" | "src/lib.rs" => Some(SYN_SITE.as_bytes()),
        _ => None,
    }
}

fn sha256_hex(bytes: &[u8]) -> std::string::String {
    let mut hasher = blaze_sha256::Sha256::new();
    hasher.update(bytes);
    hasher.finish().iter().map(|b| std::format!("{b:02x}")).collect()
}

/// turso's quickcheck_macros 1.0.0 (2026-11-21): E0599 "no method named `parse`" on
/// `syn::Item::parse.parse(input)`, because syn 1.0.109 compiles `Parser::parse` only off
/// wasm32-wasi. The real case, reduced: the checked-in row corrects that site, keeps the method,
/// and applies to syn 1.0.109 only. Control: against bytes other than the ones its digests name,
/// the row is refused by name (`Changed`, the first file listed), even though its predicate
/// still matches. Twin: the same row with the fixtures' digests corrects every file. Then a
/// digest that names an unlisted file, or is not hex, is refused when the table is read.
/// Prints each verdict.
#[test]
fn syn_1_parser_parse_is_kept_on_wasi_and_pinned_by_content() {
    let alloc = blaze_own::testing::heap().alloc();
    let table = table();
    let row = table.find_text("syn", "1.0.109").expect("the syn row applies to syn 1.0.109");
    assert!(table.find_text("syn", "1.0.108").is_none() && table.find_text("syn", "2.0.106").is_none());
    let site = row.correct_text(SYN_PARSE_RS, Cpu::SCALAR, alloc);
    assert_eq!(site.replaced.len(), 1);
    assert!(site.text.as_str().contains("not(any()),\n        feature = \"proc-macro\"\n    ))]\n    fn parse(self"), "{}", site.text.as_str());

    let refused = row.correct_crate("1.0.109", syn_file, Cpu::SCALAR, alloc).err().expect("refused: the fixtures are not syn's bytes");
    assert!(matches!(refused, Refused::Changed { file: "src/buffer.rs", version: "1.0.109", .. }), "{refused}");
    println!("syn row on other bytes: {refused}");

    let mut text = std::string::String::from(TABLE);
    for (file, real) in SYN_DIGESTS {
        text = text.replace(real, &sha256_hex(syn_file(file).expect("a fixture")));
    }
    let pinned = Table::read(&text, Cpu::SCALAR, alloc).unwrap_or_else(|error| panic!("{error}"));
    let row = pinned.find_text("syn", "1.0.109").expect("the syn row");
    let files = row.correct_crate("1.0.109", syn_file, Cpu::SCALAR, alloc).unwrap_or_else(|why| panic!("{why}"));
    let counts: std::vec::Vec<(&str, usize)> = files.iter().map(|(path, c)| (*path, c.replaced.len())).collect();
    assert_eq!(counts, [("src/buffer.rs", 1), ("src/export.rs", 1), ("src/lib.rs", 1), ("src/parse.rs", 1)]);
    println!("syn row on its pinned bytes: {counts:?}");

    let unlisted = TABLE.replace("\"file\": \"src/export.rs\"", "\"file\": \"src/token.rs\"");
    let error = Table::read(&unlisted, Cpu::SCALAR, alloc).err().expect("refused");
    assert!(std::format!("{error}").contains("src/token.rs"), "{error}");
    let short = TABLE.replace(SYN_DIGESTS[3].1, "7b2f8cad");
    let error = Table::read(&short, Cpu::SCALAR, alloc).err().expect("refused");
    assert!(std::format!("{error}").contains("src/parse.rs"), "{error}");
}

/// synstructure 0.13.2 src/lib.rs, its three gated sites (lines 158, 2478, 2493), as written.
const LIB_RS: &str = r#"#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
extern crate proc_macro;

pub trait MacroResult {
    /// *This method is available if `synstructure` is built with the
    /// `"proc-macro"` feature.*
    #[cfg(all(
        not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
        feature = "proc-macro"
    ))]
    fn into_stream(self) -> proc_macro::TokenStream
    where
        Self: Sized,
    {
        match self.into_result() {
            Ok(ts) => ts.into(),
            Err(err) => err.to_compile_error().into(),
        }
    }
}

#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
impl MacroResult for proc_macro::TokenStream {
    fn into_result(self) -> Result<TokenStream> {
        Ok(self.into())
    }
}
"#;

/// synstructure 0.13.2 src/macros.rs, its four gated sites (lines 10, 15, 84, 147).
const MACROS_RS: &str = r#"pub use syn::{parse2, parse_str, DeriveInput};

#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
pub use proc_macro::TokenStream;
#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
pub use syn::parse;

#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
#[macro_export]
macro_rules! decl_derive {
    () => {};
}

#[cfg(all(
    not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi"))),
    feature = "proc-macro"
))]
#[macro_export]
macro_rules! decl_attribute {
    () => {};
}
"#;

const GATE: &str = r#"not(all(target_arch = "wasm32", any(target_os = "unknown", target_os = "wasi")))"#;

fn table() -> Table<Alloc<'static>> {
    Table::read(TABLE, Cpu::SCALAR, blaze_own::testing::heap().alloc()).expect("the checked-in table reads")
}

fn named(table: Table<Alloc<'static>>, name: &str) -> Row<Alloc<'static>> {
    let mut rows = table.rows;
    let at = rows.iter().position(|row| row.name.as_str() == name).unwrap_or_else(|| panic!("the {name} row"));
    rows.remove_swap(at)
}

fn row() -> Row<Alloc<'static>> {
    named(table(), "synstructure")
}

fn file<'t>(path: &str) -> Option<&'t [u8]> {
    match path {
        "src/lib.rs" => Some(LIB_RS.as_bytes()),
        "src/macros.rs" => Some(MACROS_RS.as_bytes()),
        _ => None,
    }
}

#[test]
fn synstructure_seven_sites() {
    let table = table();
    let version: Version<Alloc<'static>> = Version::parse_in("0.13.2", blaze_semver::Limits::CARGO, blaze_own::testing::heap().alloc()).unwrap();
    let row = table.find("synstructure", &version).expect("the row applies to synstructure 0.13.2");
    assert!(table.find("synstructure", &Version::parse_in("0.12.6", blaze_semver::Limits::CARGO, blaze_own::testing::heap().alloc()).unwrap()).is_none());
    assert!(table.find_text("synstructure", "0.14.0").is_some());
    assert!(table.find_text("synstructure", "0.15.0").is_none());
    assert!(table.find_text("syn", "0.13.2").is_none());
    let files = row.correct_crate("0.13.2", file, Cpu::SCALAR, blaze_own::testing::heap().alloc()).expect("corrected");
    let counts: std::vec::Vec<(&str, usize)> = files.iter().map(|(path, c)| (*path, c.replaced.len())).collect();
    assert_eq!(counts, [("src/lib.rs", 3), ("src/macros.rs", 4)]);
    for (path, corrected) in files.iter() {
        let text = corrected.text.as_str();
        assert!(!text.contains("wasm32"), "{path} still gates on wasm32");
        assert!(text.contains("not(any())"), "{path}");
        let original = if *path == "src/lib.rs" { LIB_RS } else { MACROS_RS };
        for range in corrected.replaced.iter() {
            assert_eq!(&original[range.start as usize..range.end as usize], &GATE[4..GATE.len() - 1]);
        }
    }
}

#[test]
fn split_over_lines_with_a_comment() {
    let text = "#[cfg(not(all(\n  target_arch = \"wasm32\", // the old gate\n  any(target_os = \"unknown\", /* or */ target_os=\"wasi\")\n)))]\nfn f() {}\n";
    let corrected = row().correct_text(text, Cpu::SCALAR, blaze_own::testing::heap().alloc());
    assert_eq!(corrected.text.as_str(), "#[cfg(not(any()))]\nfn f() {}\n");
    assert_eq!(corrected.replaced.len(), 1);
}

#[test]
fn cfg_attr_is_corrected() {
    let text = "#![cfg_attr(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\")), allow(x))]\n";
    let corrected = row().correct_text(text, Cpu::SCALAR, blaze_own::testing::heap().alloc());
    assert_eq!(corrected.text.as_str(), "#![cfg_attr(any(), allow(x))]\n");
}

#[test]
fn near_misses_are_untouched() {
    let texts = [
        // Another target.
        "#[cfg(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasip2\")))]\nfn f() {}\n",
        // Not an attribute: the predicate as text in a string and in a call.
        "const S: &str = \"all(target_arch = \\\"wasm32\\\", any(target_os = \\\"unknown\\\", target_os = \\\"wasi\\\"))\";\n",
        "fn f() { m(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\"))); }\n",
        // A comment.
        "// #[cfg(all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\")))]\n",
        // Inside the predicate but not a whole subtree: a path ending in `all`.
        "#[cfg(x::all(target_arch = \"wasm32\", any(target_os = \"unknown\", target_os = \"wasi\")))]\n",
    ];
    for text in texts {
        let corrected = row().correct_text(text, Cpu::SCALAR, blaze_own::testing::heap().alloc());
        assert!(matches!(corrected.text, Text::Same(same) if core::ptr::eq(same, text)), "{text}");
        assert!(corrected.replaced.is_empty());
    }
}

#[test]
fn a_stale_row_is_refused() {
    let row = row();
    let fixed = |path: &str| match path {
        "src/lib.rs" => Some(LIB_RS.as_bytes()),
        "src/macros.rs" => Some(&b"pub use syn::parse;\n"[..]),
        _ => None,
    };
    let refused = row.correct_crate("0.13.3", fixed, Cpu::SCALAR, blaze_own::testing::heap().alloc()).err().expect("refused");
    assert!(matches!(refused, Refused::Stale { file: "src/macros.rs", version: "0.13.3", .. }), "{refused}");
    let missing = row.correct_crate("0.13.3", |_| None, Cpu::SCALAR, blaze_own::testing::heap().alloc()).err().expect("refused");
    assert!(matches!(missing, Refused::MissingFile { file: "src/lib.rs", .. }), "{missing}");
}

#[test]
fn unknown_fields_are_refused() {
    let text = TABLE.replace("\"issue\": \"\"", "\"issue\": \"\", \"extra\": 1");
    let error = Table::read(&text, Cpu::SCALAR, blaze_own::testing::heap().alloc()).err().expect("refused");
    assert!(std::format!("{error}").contains("extra"), "{error}");
    let unbalanced = TABLE.replace("\"replace\": \"any()\"", "\"replace\": \"any(\"");
    let error = Table::read(&unbalanced, Cpu::SCALAR, blaze_own::testing::heap().alloc()).err().expect("refused");
    assert!(std::format!("{error}").contains("`replace`"), "{error}");
}

#[test]
fn deterministic_bytes_and_digest() {
    let (ra, rb) = (row(), row());
    let a = ra.correct_crate("0.13.2", file, Cpu::SCALAR, blaze_own::testing::heap().alloc()).unwrap();
    let b = rb.correct_crate("0.13.2", file, Cpu::BASELINE, blaze_own::testing::heap().alloc()).unwrap();
    for ((pa, ca), (pb, cb)) in a.iter().zip(b.iter()) {
        assert_eq!(pa, pb);
        assert_eq!(ca.text.as_str(), cb.text.as_str());
    }
    assert_eq!(row().digest(), row().digest());
    let edited = TABLE.replace("\"replace\": \"any()\"", "\"replace\": \"all(any())\"");
    let other = named(Table::read(&edited, Cpu::SCALAR, blaze_own::testing::heap().alloc()).unwrap(), "synstructure");
    assert_ne!(row().digest(), other.digest(), "a row edit changes the derived source's key");
}
