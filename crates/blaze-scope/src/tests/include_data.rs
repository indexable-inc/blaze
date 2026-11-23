//! Which read an include asks of the crate's texts (resolve.rs `Host::include`): `include!` of
//! items reads a module file ([`SourceTexts::read`]: its `Surface` is what the names read),
//! `include_str!` and `include_bytes!` read DATA ([`SourceTexts::read_data`]: bytes, any bytes,
//! named with the file whose call read them). Before 2026-11-16 the host ignored the kind and
//! read every include as a module file; blaze-cli names.rs then wanted the `Surface` of a JSON
//! file and stopped (`Unlisted`: tempo's `crates/node/tests/assets/test-genesis.json`).

use std::string::String;
use std::sync::Mutex;
use std::vec::Vec;


use super::entry::input;
use crate::graph::Sysroot;
use crate::resolve::{SourceTexts, resolve_crates};
use crate::task::Serial;

/// core `macros/mod.rs`: `include!` "Parses a file as an expression or an item according to
/// the context"; `include_str!` "Includes a UTF-8 encoded file as a string"; `include_bytes!`
/// "Includes a file as a reference to a byte array". "The file is located relative to the
/// current file (similarly to how modules are found)" for all three.
const LIB: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! include_str { () => {} }
#[rustc_builtin_macro]
macro_rules! include_bytes { () => {} }
mod items {
    include!(\"items.rs\");
}
pub use items::A;
pub fn f() {
    include_str!(\"../tests/assets/g.json\");
    include_bytes!(\"blob.bin\");
}
pub fn g() -> u32 {
    let n = include!(\"expr.in\");
    n
}
";

static FILES: &[(&str, &[u8])] = &[
    ("/w/src/lib.rs", LIB.as_bytes()),
    ("/w/src/items.rs", b"pub struct A;\n"),
    ("/w/src/expr.in", b"{ struct B; impl B { fn n(&self) -> u32 { 7 } } B.n() }\n"),
    ("/w/tests/assets/g.json", b"{\"chainId\": 1337}\n"),
    ("/w/src/blob.bin", b"\xff\xfe\x00\x01"),
];

/// The texts, each ask recorded: module reads by path, data reads by (calling file, path).
#[derive(Default)]
struct Asked {
    modules: Mutex<Vec<String>>,
    data: Mutex<Vec<(String, String)>>,
}

fn bytes_at(path: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(at, _)| *at == path).map(|(_, bytes)| *bytes)
}

impl SourceTexts<'static> for Asked {
    fn read(&self, _krate: usize, path: &str) -> Option<&'static str> {
        self.modules.lock().unwrap_or_else(|p| panic!("modules: {p}")).push(String::from(path));
        core::str::from_utf8(bytes_at(path)?).ok()
    }

    fn read_data(&self, _krate: usize, from: &str, path: &str) -> Option<&'static [u8]> {
        self.data.lock().unwrap_or_else(|p| panic!("data: {p}")).push((String::from(from), String::from(path)));
        bytes_at(path)
    }
}

/// The kind and the place decide the read: `include!("items.rs")` in a module is a module read
/// of `/w/src/items.rs` (and `A` from it resolves); `include!("expr.in")` in `g`'s body is a
/// data read named with `/w/src/lib.rs` (one expression whose block declares `B` and its impl:
/// its whole text is what the names read); and neither data file of `f` is ever read as a
/// module. Without the `match how` in `Host::include` every include is a module read and
/// `data` is empty.
///
/// Changed 2026-11-20 (knowingly): `f`'s `include_str!` and `include_bytes!` are no longer read
/// by the names at all. They are calls in a body whose output is a literal, so they declare
/// nothing there (resolve.rs `LITERAL_BUILTINS`) and the names library does not expand them.
/// No reader needed that read: the body's own lowering reads the file (blaze-ladder
/// body_host.rs, the body's WHOLE read, which blaze-cli engine_bodies.rs `missing_data` checks),
/// and the plan lists it from the text (blaze-cli data_include.rs), not from the names' reads.
///
/// Prints `module reads: [..]; data reads: [..]`.
#[test]
fn an_include_reads_a_module_and_include_str_and_include_bytes_read_data() {
    let asked = Asked::default();
    let texts: &'static Asked = std::boxed::Box::leak(std::boxed::Box::new(asked));
    let crates = [input("w", "/w/src/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), texts, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let modules = texts.modules.lock().unwrap_or_else(|p| panic!("modules: {p}")).clone();
    let mut data = texts.data.lock().unwrap_or_else(|p| panic!("data: {p}")).clone();
    data.sort();
    data.dedup();
    std::println!("module reads: {modules:?}; data reads: {data:?}");
    assert!(modules.iter().any(|path| path == "/w/src/items.rs"), "include! of items is a module read: {modules:?}");
    assert!(!modules.iter().any(|path| path.ends_with(".json") || path.ends_with(".bin") || path.ends_with(".in")), "a data file read as a module: {modules:?}");
    assert_eq!(data, [(String::from("/w/src/lib.rs"), String::from("/w/src/expr.in"))]);
    drop(names);
}
