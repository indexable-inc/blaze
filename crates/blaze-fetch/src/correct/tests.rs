//! Each rule of the correction has a fixture that fails without it.

use blaze_lex::Cpu;
use blaze_own::Alloc;
use blaze_semver::Version;

use super::{Refused, Row, TABLE, Table, Text};

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

fn row() -> Row<Alloc<'static>> {
    let mut rows = table().rows;
    rows.pop().expect("the synstructure row")
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
    let other = Table::read(&edited, Cpu::SCALAR, blaze_own::testing::heap().alloc()).unwrap().rows.pop().unwrap();
    assert_ne!(row().digest(), other.digest(), "a row edit changes the derived source's key");
}
