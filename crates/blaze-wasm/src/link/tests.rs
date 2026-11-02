use blaze_codegen::{Relocation, wasm::Signature};
use blaze_items::CodeHash;

use super::*;

fn id(byte: u8) -> ContentId {
    ContentId([byte; 32])
}

/// A function of key `key` and version `version` whose body is `body`, with the places of
/// `relocs` at offsets into the body.
struct Made {
    key: u8,
    version: u8,
    body: Vec<u8>,
    relocs: Vec<(u32, RelocKind, Target)>,
}

fn compiled(made: &Made) -> CompiledFunction {
    let code = Code { ty: Signature::default(), ..Code::default() };
    let (bytes, start) = code.encode(&made.body);
    let relocs: Relocs = made
        .relocs
        .iter()
        .map(|(at, kind, target)| Relocation {
            at: start + at,
            kind: *kind,
            target: target.clone(),
            addend: 0,
        })
        .collect();
    CompiledFunction {
        key: InstanceKey(id(made.key)),
        arch: Arch::Wasm32,
        align: 1,
        code: bytes,
        relocs,
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

fn built(made: &[Made], statics: Vec<Static>) -> Built {
    Built {
        functions: made.iter().map(compiled).collect(),
        inputs: made.iter().map(|it| (InstanceKey(id(it.key)), CodeHash(id(it.key)))).collect(),
        versions: made.iter().map(|it| (id(it.version), Ok(id(it.version)))).collect(),
        symbols: made
            .iter()
            .map(|it| (InstanceKey(id(it.key)), format!("f{}", it.version)))
            .collect(),
        statics,
        ..Built::default()
    }
}

/// `end`.
fn empty() -> Vec<u8> {
    vec![0x00, 0x0b]
}

/// `call <index>; end`, the index at 2.
fn call() -> Vec<u8> {
    vec![0x00, 0x10, 0x80, 0x80, 0x80, 0x80, 0x00, 0x0b]
}

/// `i32.const <address>; drop; end`, the address at 2.
fn address() -> Vec<u8> {
    vec![0x00, 0x41, 0x80, 0x80, 0x80, 0x80, 0x00, 0x1a, 0x0b]
}

fn calls(key: u8) -> Vec<(u32, RelocKind, Target)> {
    vec![(2, RelocKind::WasmFunction, Target::Function(InstanceKey(id(key))))]
}

fn valid(patch: &Patch) {
    wasmparser::Validator::new().validate_all(&patch.module).expect("a valid module");
}

fn a_static(item: u8, bytes: Vec<u8>) -> Static {
    Static {
        item: ItemKey(id(item)),
        instance: id(item),
        align: 4,
        init: Init::Bytes { bytes, relocs: Relocs::default() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn reads(item: u8) -> Vec<(u32, RelocKind, Target)> {
    vec![(2, RelocKind::WasmMemory, Target::Static { item: ItemKey(id(item)), instance: id(item) })]
}

#[test]
fn a_build_again_puts_nothing() {
    let made = [
        Made { key: 1, version: 10, body: call(), relocs: calls(2) },
        Made { key: 2, version: 20, body: empty(), relocs: Vec::new() },
    ];
    let mut linker = Linker::default();
    let first = linker.link(&built(&made, Vec::new()), &[]).unwrap();
    valid(&first);
    assert_eq!(first.functions, ["f10", "f20"]);
    assert_eq!(first.exports, [("f10".to_owned(), 1), ("f20".to_owned(), 2)]);
    let again = linker.link(&built(&made, Vec::new()), &[]).unwrap();
    valid(&again);
    assert!(again.functions.is_empty(), "{:?}", again.functions);
    assert_eq!(again.exports, first.exports);
}

#[test]
fn an_edit_puts_what_changed_and_what_calls_it_directly() {
    let made = |callee: u8| {
        [
            Made { key: 1, version: 10, body: call(), relocs: calls(callee) },
            Made { key: callee, version: 20, body: empty(), relocs: Vec::new() },
            Made { key: 3, version: 30, body: empty(), relocs: Vec::new() },
        ]
    };
    let mut linker = Linker::default();
    let first = linker.link(&built(&made(2), Vec::new()), &[]).unwrap();
    assert_eq!(first.functions.len(), 3);
    // The callee's body changed: a new key, the same function across versions.
    let edited = linker.link(&built(&made(4), Vec::new()), &[]).unwrap();
    valid(&edited);
    let mut put = edited.functions.clone();
    put.sort();
    assert_eq!(put, ["f10", "f20"]);
    assert_eq!(edited.exports, first.exports, "every function keeps its slot");
}

#[test]
fn a_direct_call_to_an_earlier_module_is_imported_by_slot() {
    let mut linker = Linker::default();
    let first = [
        Made { key: 1, version: 10, body: empty(), relocs: Vec::new() },
        Made { key: 2, version: 20, body: empty(), relocs: Vec::new() },
    ];
    linker.link(&built(&first, Vec::new()), &[]).unwrap();
    // The caller changed to call the other, which is as it was.
    let second = [
        Made { key: 5, version: 10, body: call(), relocs: calls(2) },
        Made { key: 2, version: 20, body: empty(), relocs: Vec::new() },
    ];
    let patch = linker.link(&built(&second, Vec::new()), &[]).unwrap();
    valid(&patch);
    assert_eq!(patch.functions, ["f10"]);
    let imports: Vec<(String, String)> = wasmparser::Parser::new(0)
        .parse_all(&patch.module)
        .filter_map(|it| match it.unwrap() {
            wasmparser::Payload::ImportSection(reader) => Some(
                reader
                    .into_imports()
                    .map(|it| {
                        let it = it.unwrap();
                        (it.module.to_owned(), it.name.to_owned())
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(imports.contains(&(SLOT.to_owned(), "2".to_owned())), "{imports:?}");
}

#[test]
fn a_static_changed_in_place_leaves_its_readers() {
    let made = [Made { key: 1, version: 10, body: address(), relocs: reads(7) }];
    let mut linker = Linker::default();
    let first = linker.link(&built(&made, vec![a_static(7, vec![1, 0, 0, 0])]), &[]).unwrap();
    valid(&first);
    assert_eq!(first.data_len, 4);
    let same_size = linker.link(&built(&made, vec![a_static(7, vec![2, 0, 0, 0])]), &[]).unwrap();
    valid(&same_size);
    assert!(same_size.functions.is_empty(), "{:?}", same_size.functions);
    assert_eq!(same_size.data_len, 4);
    assert_eq!(same_size.memory_len, first.memory_len);
}

#[test]
fn a_static_that_grew_moves_and_its_readers_are_put_again() {
    let made = [Made { key: 1, version: 10, body: address(), relocs: reads(7) }];
    let mut linker = Linker::default();
    let first = linker.link(&built(&made, vec![a_static(7, vec![1, 0, 0, 0])]), &[]).unwrap();
    let grown = linker.link(&built(&made, vec![a_static(7, vec![1; 64])]), &[]).unwrap();
    valid(&grown);
    assert_eq!(grown.functions, ["f10"]);
    assert!(grown.memory_len > first.memory_len);
}

#[test]
fn a_call_to_nothing_is_an_error() {
    let made = [Made {
        key: 1,
        version: 10,
        body: call(),
        relocs: vec![(2, RelocKind::WasmFunction, Target::Extern("nowhere".to_owned()))],
    }];
    let error = Linker::default().link(&built(&made, Vec::new()), &[]).unwrap_err();
    assert!(error.contains("`nowhere`"), "{error}");
}

#[test]
fn a_position_is_a_location_in_memory() {
    let table = {
        let mut out = b"BZSPANS1".to_vec();
        out.extend(1u32.to_le_bytes());
        out.extend(7u32.to_le_bytes());
        out.extend(b"main.rs");
        out.extend(1u32.to_le_bytes());
        for word in [0u32, 3, 9] {
            out.extend(word.to_le_bytes());
        }
        out
    };
    let span = SpanRef { anchor: id(99), index: 0 };
    let made = [Made {
        key: 1,
        version: 10,
        body: address(),
        relocs: vec![(2, RelocKind::WasmMemory, Target::Position(span))],
    }];
    let patch = Linker::default().link(&built(&made, Vec::new()), &[(id(99), table)]).unwrap();
    valid(&patch);
    // The name with its NUL, and the record.
    assert_eq!(patch.data_len, 8 + LOCATION_SIZE as usize);
    let missing = Linker::default().link(&built(&made, Vec::new()), &[]).unwrap_err();
    assert!(missing.contains("span table"), "{missing}");
}

#[test]
fn a_program_in_another_module_starts_where_it_is_told() {
    let made = [Made { key: 1, version: 10, body: empty(), relocs: Vec::new() }];
    let mut linker = Linker::default();
    linker.set_start(100, 1 << 24).unwrap();
    let patch = linker.link(&built(&made, vec![a_static(7, vec![1, 0, 0, 0])]), &[]).unwrap();
    assert_eq!(patch.exports, [("f10".to_owned(), 100)]);
    assert_eq!(patch.table_len, 101);
    assert_eq!(patch.memory_len, (1 << 24) + 4);
    assert!(linker.set_start(0, 0).is_err(), "nothing moves once linked");
}

#[test]
fn a_truncated_span_table_is_refused() {
    assert!(SpanTable::decode(b"BZSPANS1\x01\0\0\0\x09\0\0\0main").is_err());
    assert!(SpanTable::decode(b"nope").is_err());
}
