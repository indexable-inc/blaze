use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use super::*;
use crate::{CompiledFunction, RelocKind, Relocation, Target, wasm::ValType};

fn id(n: u8) -> ContentId {
    ContentId([n; 32])
}

fn base() -> Static {
    Static {
        item: ItemKey(id(1)),
        instance: id(2),
        align: 8,
        init: Init::Bytes { bytes: vec![7; 16], relocs: crate::Relocs::new() },
        data: vec![Data::new(4, vec![1, 2, 3, 4], crate::Relocs::new())],
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn relocs(target: Target) -> crate::Relocs {
    vec![Relocation { at: 0, kind: RelocKind::Abs64, target, addend: 0 }].into()
}

fn import(field: &str, params: Vec<ValType>) -> (String, wasm::Import) {
    let ty = wasm::Signature { params, results: vec![ValType::I32] };
    ("host_fn".to_owned(), wasm::Import { module: "env".to_owned(), field: field.to_owned(), ty })
}

#[test]
fn a_static_definition_changes_with_every_field_it_covers() {
    let before = base();
    let changed: Vec<(&str, Static)> = vec![
        ("its item", Static { item: ItemKey(id(9)), ..base() }),
        ("its instance", Static { instance: id(9), ..base() }),
        ("its alignment", Static { align: 16, ..base() }),
        ("its value", Static { init: Init::Bytes { bytes: vec![8; 16], relocs: crate::Relocs::new() }, ..base() }),
        (
            "its relocations",
            Static { init: Init::Bytes { bytes: vec![7; 16], relocs: relocs(Target::Data(id(5))) }, ..base() },
        ),
        (
            "what a relocation names",
            Static { init: Init::Bytes { bytes: vec![7; 16], relocs: relocs(Target::Data(id(6))) }, ..base() },
        ),
        ("zeroed", Static { init: Init::Zeroed { size: 16 }, ..base() }),
        ("its data's bytes", Static { data: vec![Data::new(4, vec![1, 2, 3, 5], crate::Relocs::new())], ..base() }),
        ("its data's alignment", Static { data: vec![Data::new(8, vec![1, 2, 3, 4], crate::Relocs::new())], ..base() }),
        ("no data", Static { data: Vec::new(), ..base() }),
        ("its mutability", Static { mutable: true, ..base() }),
        ("its thread-locality", Static { thread_local: true, ..base() }),
        ("its section", Static { section: Some(".init_array".to_owned()), ..base() }),
        ("an import", Static { imports: vec![import("f", vec![ValType::I32])], ..base() }),
    ];
    let digest = static_definition(&before);
    assert_eq!(digest, static_definition(&base()), "a function of the static alone");
    let mut seen = vec![digest];
    for (what, value) in &changed {
        let it = static_definition(value);
        assert!(!seen.contains(&it), "{what} is not in the static's definition");
        seen.push(it);
    }

    let with = |field: &str, params: Vec<ValType>| {
        static_definition(&Static { imports: vec![import(field, params)], ..base() })
    };
    let one = with("f", vec![ValType::I32]);
    assert_ne!(one, with("g", vec![ValType::I32]), "an import's field");
    assert_ne!(one, with("f", vec![ValType::I64]), "an import's type");
    assert_ne!(one, with("f", vec![ValType::I32, ValType::I32]), "an import's arity");
    let mut renamed = import("f", vec![ValType::I32]);
    renamed.0 = "other".to_owned();
    assert_ne!(one, static_definition(&Static { imports: vec![renamed], ..base() }), "an import's extern");
    let mut module = import("f", vec![ValType::I32]);
    module.1.module = "wasi".to_owned();
    assert_ne!(one, static_definition(&Static { imports: vec![module], ..base() }), "an import's module");
}

#[test]
fn a_function_digest_is_its_code_hash_and_what_it_built_in() {
    let (code, other) = (CodeHash(id(1)), CodeHash(id(2)));
    assert_eq!(function_digest(code, &[]), code.0, "nothing built in hashes nothing");
    let built_in = function_digest(code, &[other]);
    assert_ne!(built_in, code.0);
    assert_ne!(built_in, function_digest(other, &[other]), "the code hash");
    assert_ne!(built_in, function_digest(code, &[code]), "what it built in");
    assert_ne!(built_in, function_digest(code, &[other, code]), "more built in");
    assert_eq!(built_in, function_digest(code, &[other]));
}

#[test]
fn a_debug_name_digest_is_the_names() {
    assert_eq!(debug_name_digest("lib::f"), debug_name_digest("lib::f"));
    assert_ne!(debug_name_digest("lib::f"), debug_name_digest("lib::g"));
}

#[test]
fn only_the_kept_functions_have_digests_sorted_by_key() {
    let function = |n: u8| CompiledFunction {
        key: InstanceKey(id(n)),
        arch: crate::Arch::X86_64,
        align: 16,
        code: vec![n],
        relocs: crate::Relocs::new(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    };
    let kept = [function(1), function(3), function(5)];
    let digests = [5, 2, 1, 3, 4, 1].map(|n| (InstanceKey(id(n)), id(n + 100))).to_vec();
    let got = of_kept(digests, &kept);
    assert_eq!(got, [1, 3, 5].map(|n| (InstanceKey(id(n)), id(n + 100))));
}
