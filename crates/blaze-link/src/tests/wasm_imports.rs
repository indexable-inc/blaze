//! What codegen's functions and statics say a name is imported as on wasm, through whole wasm
//! links: the import the name's live declarers agree on, an error naming both when two
//! disagree, and an edit that moves a name to another module once the old declarer is gone.

use blaze_codegen::wasm::{Code, Import, Signature};
use blaze_codegen::{Arch, CompiledFunction, Init, RelocKind, Relocation, Relocs, Static, Target};
use blaze_items::ItemKey;

use super::{Change, Disk, Image, Output, OutputKind, TO, function, id, key, name, output};
use crate::{Format, LinkError};

/// The entry point and the functions importing [`PUTS`].
const ENTRY: u8 = 1;
const F: u8 = 2;
const G: u8 = 3;
const G2: u8 = 4;

const PUTS: &str = "puts";

fn wasm() -> Output {
    Output {
        format: Format::Wasm,
        arch: Arch::Wasm32,
        kind: OutputKind::Executable { entry: "_start".into() },
        interpreter: None,
        triple: "wasm32-unknown-unknown".into(),
        ..output(false)
    }
}

fn import(module: &str) -> Import {
    Import { module: module.to_owned(), field: PUTS.to_owned(), ty: Signature::default() }
}

/// Function `n` of `code`'s header and `body` (no locals, the instructions, `end`).
fn wasm_function(n: u8, code: &Code, body: &[u8], relocs: impl Fn(u32) -> Vec<Relocation>) -> CompiledFunction {
    let (code, start) = code.encode(body);
    CompiledFunction { arch: Arch::Wasm32, align: 1, ..function(n, &code, relocs(start)) }
}

fn entry() -> CompiledFunction {
    wasm_function(ENTRY, &Code::default(), &[0x00, 0x0b], |_| Vec::new())
}

/// Function `n`, calling [`PUTS`], which it says is `module`'s: `call` and the function's
/// index as a 5-byte padded LEB the linker fills.
fn calling(n: u8, module: &str) -> CompiledFunction {
    let code = Code { imports: vec![(PUTS.to_owned(), import(module))], ..Code::default() };
    let body = [0x00, 0x10, 0x80, 0x80, 0x80, 0x80, 0x00, 0x0b];
    wasm_function(n, &code, &body, |start| {
        vec![Relocation { at: start + 2, kind: RelocKind::WasmFunction, target: Target::Extern(PUTS.into()), addend: 0 }]
    })
}

/// A static saying [`PUTS`] is `module`'s.
fn declaring(module: &str) -> Static {
    Static {
        item: ItemKey(id(20)),
        instance: id(120),
        align: 4,
        init: Init::Bytes { bytes: vec![0; 4], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: vec![(PUTS.to_owned(), import(module))],
    }
}

fn program<'a>(functions: &[&'a CompiledFunction]) -> Change<'a> {
    Change {
        added: functions.to_vec(),
        names: vec![("_start".into(), Target::Direct(key(ENTRY)))],
        ..Change::default()
    }
}

/// What the image says [`PUTS`] is imported as, and how many live pieces say so.
fn puts(image: &Image) -> (Option<&Import>, u32) {
    let Some(name) = image.symbols.find_name(PUTS) else { return (None, 0) };
    (image.state.wasm_imports.get(&name), image.state.wasm_declarers(name))
}

/// The linked module.
fn module(disk: &Disk) -> Vec<u8> {
    let it = disk.file(TO);
    (0..it.len).map(|at| it.page(at / super::PAGE)[(at % super::PAGE) as usize]).collect()
}

/// Whether the module imports [`PUTS`] from `module`: the import section's module and field
/// names, each its length and bytes.
fn imports_from(bytes: &[u8], module: &str) -> bool {
    let mut entry = vec![module.len() as u8];
    entry.extend_from_slice(module.as_bytes());
    entry.push(PUTS.len() as u8);
    entry.extend_from_slice(PUTS.as_bytes());
    bytes.windows(entry.len()).any(|it| it == entry)
}

fn says_both(error: &LinkError, first: &str, second: &str) {
    let what = &error.what;
    assert!(what.contains("`puts`"), "{what}");
    let (first, second) = (format!("`{first}`.`puts`"), format!("`{second}`.`puts`"));
    let (a, b) = (what.find(&first), what.find(&second));
    assert!(a.is_some() && b.is_some() && a < b, "{what}");
}

#[test]
fn two_functions_importing_one_name_from_two_modules_is_an_error_naming_both() {
    let (start, f, g) = (entry(), calling(F, "env_a"), calling(G, "env_b"));

    let mut image = Image::new(name(TO), wasm()).unwrap();
    let error = image.stage(program(&[&start, &f, &g])).err().expect("two imports of `puts`");
    says_both(&error, "env_a", "env_b");
    assert_eq!(error.function, Some(key(G)));
    assert!(error.what.contains(&format!("the function {:?}", key(F))), "{}", error.what);
    assert!(error.what.contains(&format!("the function {:?}", key(G))), "{}", error.what);

    // The same across links: the first stays as it was.
    let mut disk = Disk::default();
    disk.link(&mut image, program(&[&start, &f])).unwrap();
    let error = image.stage(Change { added: vec![&g], ..Change::default() }).err().expect("a second import of `puts`");
    says_both(&error, "env_a", "env_b");
    assert_eq!(error.function, Some(key(G)));
    assert_eq!(puts(&image), (Some(&import("env_a")), 1));

    // A static is a declarer the same way.
    let error = image.stage(Change { statics: vec![&declaring("env_b")], ..Change::default() }).err().expect("a static's");
    says_both(&error, "env_a", "env_b");
    assert_eq!(error.function, None);
    assert!(error.what.contains(&format!("by the function {:?}", key(F))), "{}", error.what);
    assert!(error.what.contains("by the static"), "{}", error.what);

    // A different type is a different import too, and says so.
    let other = Import { ty: Signature { params: vec![blaze_codegen::wasm::ValType::I32], results: Vec::new() }, ..import("env_a") };
    let code = Code { imports: vec![(PUTS.to_owned(), other)], ..Code::default() };
    let typed = wasm_function(G, &code, &[0x00, 0x0b], |_| Vec::new());
    let error = image.stage(Change { added: vec![&typed], ..Change::default() }).err().expect("another type");
    assert!(error.what.contains("of type"), "{}", error.what);
}

#[test]
fn the_same_import_declared_twice_is_one_import_until_neither_declares_it() {
    let (start, f, g) = (entry(), calling(F, "env_a"), calling(G, "env_a"));
    let mut image = Image::new(name(TO), wasm()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, program(&[&start, &f, &g])).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_a")), 2));
    assert!(imports_from(&module(&disk), "env_a"));

    disk.link(&mut image, Change { removed: vec![key(F)], ..Change::default() }).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_a")), 1));
    disk.link(&mut image, Change { removed: vec![key(G)], ..Change::default() }).unwrap();
    assert_eq!(puts(&image), (None, 0));
    assert!(!imports_from(&module(&disk), "env_a"));

    // Gone, `puts` may be another module's.
    let g2 = calling(G2, "env_b");
    disk.link(&mut image, Change { added: vec![&g2], ..Change::default() }).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_b")), 1));
}

#[test]
fn replacing_a_function_with_one_importing_from_another_module_moves_the_import() {
    let (start, g, g2) = (entry(), calling(G, "env_a"), calling(G2, "env_b"));
    let mut image = Image::new(name(TO), wasm()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, program(&[&start, &g])).unwrap();
    assert!(imports_from(&module(&disk), "env_a"));

    disk.link(&mut image, Change { removed: vec![key(G)], added: vec![&g2], ..Change::default() }).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_b")), 1));
    let bytes = module(&disk);
    assert!(imports_from(&bytes, "env_b") && !imports_from(&bytes, "env_a"));

    // A rollback gives the old import back with its declarer.
    disk.roll_back(&mut image).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_a")), 1));

    // A static edited the same way: the old one is unmade before the new one declares.
    disk.link(&mut image, Change { statics: vec![&declaring("env_a")], ..Change::default() }).unwrap();
    assert_eq!(puts(&image), (Some(&import("env_a")), 2));
    disk.link(&mut image, Change { removed: vec![key(G)], statics: vec![&declaring("env_b")], ..Change::default() })
        .unwrap();
    assert_eq!(puts(&image), (Some(&import("env_b")), 1));
}
