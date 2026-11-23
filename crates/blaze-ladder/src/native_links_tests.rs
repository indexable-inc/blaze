//! The link table's bytes and the program image made from tables (native_links.rs). Texts
//! are read by links.rs's `text_links` (one text a crate, keys by item index): the same
//! `Gather` walk the `CrateScope` job's `scope_links` runs over its files and expansions.
//!
//! Sources: Reference, items.extern.attributes.link (`#[link(name = "z")]` names a native
//! library for the extern block); abi.no_mangle ("the item's symbol name is its identifier"),
//! and an unmangled item is exported "regardless of its visibility", so a PRIVATE
//! `#[no_mangle] fn` is an export too.

use blaze_hash::FixedState;
use blaze_ids::producer::{Producers, Whole};
use blaze_native::Platform;
use blaze_own::{AVec, Alloc, Bump};

use super::{ProgramCrate, Source, Table, open_table, program_image, program_key, program_libraries, table_digest, write_table};
use crate::front2::links::{CrateLinks, LinkKind, text_links};

fn table_of<'a>(text: &'a str, bump: Bump<'a>) -> AVec<'a, u8> {
    let links = text_links("app", text, &[], bump).unwrap_or_else(|e| panic!("the text's links: {e:?}"));
    let mut out = AVec::new_in(bump.alloc());
    write_table(&links, &mut out).unwrap_or_else(|e| panic!("the table writes: {e:?}"));
    out
}

fn links_of<'a>(table: &'a [u8], alloc: Alloc<'a>) -> CrateLinks<'a> {
    match open_table(table, alloc).unwrap_or_else(|e| panic!("the table reads back: {e:?}")) {
        Table::Links(links) => links,
        Table::Refused(words) => panic!("a refused table: {words}"),
    }
}

const ONE_LINK: &str = "#[link(name = \"z\")]\nextern \"C\" { fn deflate(); }\n\
                        #[unsafe(no_mangle)] pub extern \"C\" fn rust_cb() -> i32 { 1 }\n";

/// A crate with one `#[link(name = "z")]` and one `#[no_mangle]` export: its table holds the
/// link (dylib, no modifiers), the foreign root `deflate` and the export `rust_cb`, and reads
/// back to the same lists.
///
/// Prints `native_links: table of one crate: link z (Dylib), foreign [deflate], exports [rust_cb]`.
#[test]
fn a_links_table_holds_link_z_and_a_no_mangle_export() {
    // The test's one allocator, made at its top: a frame, its memory from the OS.
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let table = table_of(ONE_LINK, bump);
    let links = links_of(&table, alloc);
    assert_eq!(links.decls.len(), 1, "one #[link]");
    assert_eq!((links.decls[0].name, links.decls[0].kind), ("z", LinkKind::Dylib));
    assert_eq!(links.foreign.as_slice(), &["deflate"][..]);
    let exports: std::vec::Vec<&str> = links.exports.iter().map(|e| e.name).collect();
    assert_eq!(exports, ["rust_cb"]);
    let mut again = AVec::new_in(alloc);
    write_table(&links, &mut again).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(table.as_slice(), again.as_slice(), "a table read back writes the same bytes");
    println!("native_links: table of one crate: link {} ({:?}), foreign [{}], exports [{}]", links.decls[0].name, links.decls[0].kind, links.foreign[0], exports[0]);
}

/// The `Links` part's source is the table's digest: a body edit and a private fn without
/// `#[no_mangle]` leave it equal (no reader moves); a PRIVATE `#[no_mangle] fn` added moves it
/// and is in the table.
///
/// CONTROL (main runs it in a round): in links.rs `Gather::exported`, return `Ok(())` at the top
/// when the item is not written `pub` (a table from the exports alone). This test must then
/// fail at "the private export is in the table".
///
/// Prints `native_links: digest equal after a body edit and a private fn; moved by a private #[no_mangle] fn (exports [hidden, rust_cb])`.
#[test]
fn a_private_no_mangle_fn_moves_the_table_a_body_edit_does_not() {
    // The test's one allocator, made at its top: a frame, its memory from the OS.
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let body = "#[link(name = \"z\")]\nextern \"C\" { fn deflate(); }\n\
                #[unsafe(no_mangle)] pub extern \"C\" fn rust_cb() -> i32 { let two = 2; two - 1 }\n";
    let private_fn = "#[link(name = \"z\")]\nextern \"C\" { fn deflate(); }\n\
                      #[unsafe(no_mangle)] pub extern \"C\" fn rust_cb() -> i32 { 1 }\nfn helper() {}\n";
    let private_export = "#[link(name = \"z\")]\nextern \"C\" { fn deflate(); }\n\
                          #[unsafe(no_mangle)] pub extern \"C\" fn rust_cb() -> i32 { 1 }\n\
                          #[unsafe(no_mangle)] extern \"C\" fn hidden() {}\n";
    let base = table_digest(&table_of(ONE_LINK, bump));
    assert_eq!(base, table_digest(&table_of(body, bump)), "a body edit moved the table");
    assert_eq!(base, table_digest(&table_of(private_fn, bump)), "a private fn without an export moved the table");
    let moved = table_of(private_export, bump);
    let links = links_of(&moved, alloc);
    let exports: std::vec::Vec<&str> = links.exports.iter().map(|e| e.name).collect();
    assert!(exports.contains(&"hidden"), "the private export is in the table: {exports:?}");
    assert_ne!(base, table_digest(&moved), "a private #[no_mangle] fn left the table's digest");
    println!("native_links: digest equal after a body edit and a private fn; moved by a private #[no_mangle] fn (exports [{}])", exports.join(", "));
}

/// A program with no C input (no `#[link]`, no extern block, no script) still gets a real
/// image: natives.rs names the system library alone, and the bytes are a dylib with no input.
///
/// Prints `native_links: a program with no C input: 0 inputs, <n> bytes, key <16 hex>`.
#[test]
fn a_program_with_no_c_input_gets_a_system_only_image() {
    // The test's one allocator, made at its top: a frame, its memory from the OS.
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let producers = Producers::for_tests(b"native_links");
    let producer = producers.get::<Whole>();
    let plain = "pub fn main() { let _ = 1 + 1; }\n";
    let table = table_of(plain, bump);
    let links = links_of(&table, alloc);
    assert!(links.decls.is_empty() && links.foreign.is_empty() && links.exports.is_empty());
    let crates = [ProgramCrate { package: "app", outputs: None, links: &links }];
    // The one library asked, `c`, resolved as the SDK's `usr/lib/libc.tbd` (libSystem's) states it.
    let asked = program_libraries(&crates, Platform::MacosArm64, &producer, alloc).unwrap_or_else(|e| panic!("the libraries: {e:?}"));
    let [one] = asked.as_slice() else { panic!("a program with no C input asks for `c` alone: {asked:?}") };
    let Source::Stub(at) = one.source else { panic!("macOS names `c` by its stub") };
    let stub: &[u8] = b"--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libSystem.B.dylib'\n...\n";
    let installs = [at.linked(stub).unwrap_or_else(|e| panic!("the stub: {e}"))];
    let key = program_key(&crates, &installs, Platform::MacosArm64, &producer, alloc).unwrap_or_else(|e| panic!("the key: {e:?}"));
    let mut bytes = AVec::new_in(alloc);
    let summary = program_image(&crates, &installs, Platform::MacosArm64, &producer, FixedState, &mut bytes, bump)
        .unwrap_or_else(|e| panic!("a program with no C input is refused: {e:?}"));
    assert_eq!(summary.inputs, 0, "no archive or object on the link line");
    assert!(!bytes.is_empty(), "no empty case: the image is written");
    let hex: std::string::String = key.0[..8].iter().map(|b| std::format!("{b:02x}")).collect();
    println!("native_links: a program with no C input: {} inputs, {} bytes, key {hex}", summary.inputs, bytes.len());
}

/// On Linux ARM64, a program with no C input produces a real ELF shared object (via
/// `blaze_export::native_so::write`) whose BNL2 descriptor reads back cleanly with 0 words,
/// inits, and pointer rows, instead of failing with `Write(NotMac)`.
#[test]
fn linux_arm64_no_c_input_writes_elf_so() {
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let producers = Producers::for_tests(b"native_links_linux");
    let producer = producers.get::<Whole>();
    let table = table_of("pub fn main() { let _ = 1 + 1; }\n", bump);
    let links = links_of(&table, alloc);
    let crates = [ProgramCrate { package: "app", outputs: None, links: &links }];
    let asked = program_libraries(&crates, Platform::LinuxArm64, &producer, alloc).unwrap_or_else(|e| panic!("linux libraries: {e:?}"));
    let [one] = asked.as_slice() else { panic!("a program with no C input asks for c alone: {asked:?}") };
    assert!(matches!(one.source, Source::Linked(_)), "Linux resolves c directly without SDK stubs");
    let mut bytes = AVec::new_in(alloc);
    let summary = program_image(&crates, &[], Platform::LinuxArm64, &producer, FixedState, &mut bytes, bump)
        .unwrap_or_else(|e| panic!("linux arm64 image refused: {e:?}"));
    assert_eq!(summary.inputs, 0);
    assert!(bytes.starts_with(b"\x7fELF"), "expected ELF header");
    let (desc, names) = blaze_native::descriptor::read(&bytes, Platform::LinuxArm64)
        .unwrap_or_else(|e| panic!("linux BNL2 descriptor unread: {e:?}"));
    assert_eq!((desc.words, desc.inits, desc.pointers, names.len()), (0, 0, 0, 0));
}
