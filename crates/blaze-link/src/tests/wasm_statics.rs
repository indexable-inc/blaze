//! The `blaze.statics` section (`crate::wasm::STATICS`): each static a wasm program's code can
//! write, listed with its item key, at the address the link placed it, beside `__heap_base`,
//! so a host can watch and carry a proc macro's state between calls.

use blaze_codegen::{Arch, Data, Init, Relocs, Static, Target};
use blaze_items::ItemKey;

use super::{Change, Disk, Image, Output, OutputKind, TO, id, key, name, output};
use crate::Format;
use crate::symbols::SymKey;

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

fn a_static(n: u8, mutable: bool, data: Vec<Data>) -> Static {
    Static {
        item: ItemKey(id(n)),
        instance: id(100 + n),
        align: 4,
        init: Init::Bytes { bytes: vec![n, 0, 0, 0], relocs: Relocs::new() },
        data,
        mutable,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

/// The payload of the module's custom section `wanted`, read section by section.
fn custom<'b>(module: &'b [u8], wanted: &str) -> Option<&'b [u8]> {
    let uleb = |at: &mut usize| {
        let (mut value, mut shift) = (0usize, 0);
        loop {
            let byte = module[*at];
            *at += 1;
            value |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return value;
            }
        }
    };
    let mut at = 8;
    while at < module.len() {
        let kind = module[at];
        at += 1;
        let len = uleb(&mut at);
        let end = at + len;
        if kind == 0 {
            let mut name_at = at;
            let name_len = uleb(&mut name_at);
            if &module[name_at..name_at + name_len] == wanted.as_bytes() {
                return Some(&module[name_at + name_len..end]);
            }
        }
        at = end;
    }
    None
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[test]
fn the_statics_section_lists_each_writable_static_at_its_placed_address() {
    let start = super::wasm_imports::entry();
    let writable =
        Data { id: id(40), align: 4, bytes: vec![9; 8], relocs: Relocs::new(), writable: true };
    let read_only =
        Data { id: id(41), align: 4, bytes: vec![8; 4], relocs: Relocs::new(), writable: false };
    let counter = a_static(20, true, vec![writable, read_only]);
    let constant = a_static(21, false, Vec::new());
    let change = Change {
        added: vec![&start],
        statics: vec![&counter, &constant],
        names: vec![("_start".into(), Target::Direct(key(super::wasm_imports::ENTRY)))],
        ..Change::default()
    };
    let mut image = Image::new(name(TO), wasm()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, change).unwrap();
    let module = super::wasm_imports::module(&disk);
    let list = custom(&module, crate::wasm::STATICS).expect("a `blaze.statics` section");

    // One entry: the counter's own 4 bytes, then its writable allocation; the constant and the
    // read-only allocation are not listed.
    assert_eq!(
        list.len(),
        crate::wasm::STATICS_HEADER + 2 * crate::wasm::STATICS_RECORD,
        "{list:?}"
    );
    assert_eq!(list[4..36], id(20).0, "the counter's item key");
    assert_eq!(word(list, 36), 2, "its records");
    let symbols = &image.symbols;
    let at = |key| image.state.addr_of(symbols.find(key).unwrap()).unwrap();
    let own =
        at(SymKey::Static(symbols.find_id(&id(20)).unwrap(), symbols.find_id(&id(120)).unwrap()));
    let data = at(SymKey::Data(symbols.find_id(&id(40)).unwrap()));
    assert_eq!(
        (u64::from(word(list, 40)), word(list, 44)),
        (own, 4),
        "the counter where it is placed"
    );
    assert_eq!((u64::from(word(list, 48)), word(list, 52)), (data, 8), "its writable allocation");
    let heap_base = u64::from(word(list, 0));
    assert!(
        own + 4 <= heap_base && data + 8 <= heap_base,
        "{own} {data} below the heap at {heap_base}"
    );
}
