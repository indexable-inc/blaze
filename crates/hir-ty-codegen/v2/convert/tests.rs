use blaze_codegen::{Arch, InstanceKey, RelocKind, Target};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use cranelift_codegen::{binemit, ir::Signature, isa::CallConv};
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget};

use super::{DataTarget, Resolve, function, value};
use crate::{
    ConstBlob, DataIdentity,
    debuginfo::{LineRow, SlotPlaces},
    fragment::{Code, DataRequest, FuncRequest, FuncTarget, Relocs, Requests},
};

fn sig() -> Signature {
    Signature::new(CallConv::SystemV)
}

fn blob(bytes: &[u8], relocs: Vec<(u32, DataId, i64)>, fn_relocs: Vec<(u32, FuncId)>) -> ConstBlob {
    ConstBlob { bytes: bytes.to_vec(), relocs, fn_relocs, align: 8, type_ids: Vec::new() }
}

fn code(relocs: Vec<ModuleReloc>, lines: Vec<LineRow>) -> Code {
    Code {
        signature: sig(),
        bytes: vec![0x90; 32],
        align: 16,
        relocs: Relocs::of_module(&relocs, &[]).unwrap(),
        unwind: None,
        lines,
        slots: SlotPlaces::default(),
    }
}

fn reloc(offset: u32, kind: binemit::Reloc, namespace: u32, index: u32) -> ModuleReloc {
    ModuleReloc { offset, kind, name: ModuleRelocTarget::User { namespace, index }, addend: -4 }
}

fn key() -> InstanceKey {
    InstanceKey(ContentId::of("test", b"key"))
}

fn callee() -> Target {
    Target::Function(InstanceKey(ContentId::of("test", b"callee")))
}

fn span(index: u32) -> SpanRef {
    SpanRef { anchor: ContentId::of("test", b"anchor"), index }
}

/// A function that calls `callee` and loads a constant that points at another constant and
/// at `callee`, with a static and a position.
fn requests() -> Requests {
    let inner = blob(b"inner\0\0\0", Vec::new(), Vec::new());
    let outer = blob(&[0; 16], vec![(0, DataId::from_u32(0), 2)], vec![(8, FuncId::from_u32(0))]);
    Requests {
        funcs: vec![FuncRequest { target: FuncTarget::Symbol("callee".to_owned()), sig: sig() }],
        datas: vec![
            DataRequest::Blob { blob: inner, identity: DataIdentity::Shared },
            DataRequest::Blob { blob: outer, identity: DataIdentity::Shared },
            DataRequest::Location(None),
            DataRequest::Location(None),
        ],
    }
}

fn convert(code: Code, requests: &Requests) -> Result<blaze_codegen::CompiledFunction, String> {
    let isa = &**crate::target().expect("the host's isa");
    let func = |_: u32, _: &FuncRequest| Ok(callee());
    let data = |index: u32, _: &DataRequest| {
        Ok(match index {
            2 => DataTarget::Target(Target::Position(span(7))),
            3 => DataTarget::Nowhere,
            _ => DataTarget::Own,
        })
    };
    let symbol = |name: &str| Ok(Target::Extern(name.to_owned()));
    let body = || Ok(Target::Extern("body".to_owned()));
    let resolve = Resolve { func: &func, data: &data, symbol: &symbol, body: &body };
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        let none = &crate::v2::wasm::Header::default();
        function(frame, key(), Arch::X86_64, isa, code, requests, &resolve, &[span(0), span(1)], None, none)
    })
}

/// A warm conversion allocates what the function keeps and what building its relocation
/// table takes: its code moves in, `used` and the data targets are the frame's, and the lists
/// Cranelift's emit wrote go back to the thread. Of the 5, 3 are kept: the table's rows and
/// targets, and the lines. The other 2 are the table of names' rows and targets, a
/// `blaze_codegen::Relocs` (on the global heap); the index, order and remap of building and
/// mapping it are the frame's.
#[test]
fn a_warm_conversion_allocates_only_what_it_keeps() {
    let make = || {
        code(
            vec![reloc(1, binemit::Reloc::X86CallPLTRel4, 0, 0)],
            vec![LineRow { offset: 0, span: 0 }, LineRow { offset: 4, span: 1 }],
        )
    };
    let requests = requests();
    convert(make(), &requests).expect("converted");
    let code = make();
    let compiled = blaze_alloc_count::assert_at_most(5, "a warm conversion: its relocations and its lines", || {
        convert(code, &requests)
    })
    .expect("converted");
    assert_eq!(compiled.relocs.len(), 1);
    assert_eq!(compiled.lines.len(), 2);
    assert!(compiled.data.is_empty());
}

#[test]
fn relocations_point_at_their_targets() {
    let code = code(
        vec![
            reloc(1, binemit::Reloc::X86CallPLTRel4, 0, 0),
            reloc(8, binemit::Reloc::X86PCRel4, 1, 1),
            reloc(16, binemit::Reloc::X86PCRel4, 1, 2),
        ],
        Vec::new(),
    );
    let compiled = convert(code, &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| it.target.clone()).collect();
    assert_eq!(targets[0], callee());
    assert_eq!(targets[1], Target::Data(compiled.data[1].id));
    assert_eq!(targets[2], Target::Position(span(7)));
    assert!(compiled.relocs.iter().all(|it| it.kind == RelocKind::PcRel32 && it.addend == -4));
}

/// Code that loads `datas`.
fn loading(datas: &[u32]) -> Code {
    let relocs = datas
        .iter()
        .enumerate()
        .map(|(at, &data)| reloc(4 * at as u32, binemit::Reloc::X86PCRel4, 1, data))
        .collect();
    code(relocs, Vec::new())
}

#[test]
fn data_points_only_at_data_before_it() {
    let compiled = convert(loading(&[1, 3]), &requests()).expect("converted");
    for (index, data) in compiled.data.iter().enumerate() {
        for reloc in &data.relocs {
            if let Target::Data(id) = &reloc.target {
                let at = compiled.data.iter().position(|it| it.id == *id).expect("own data");
                assert!(at < index, "data {index} points at data {at}");
            }
        }
    }
    // The outer constant: the inner one at 0 with its addend, the callee at 8.
    let outer = &compiled.data[1];
    assert_eq!(*outer.relocs.get(0).expect("the inner constant").target, Target::Data(compiled.data[0].id));
    assert_eq!(outer.relocs.addend(0), 2);
    assert_eq!(*outer.relocs.get(1).expect("the callee").target, callee());
    // The location of nowhere: its name, then the location pointing at it.
    assert_eq!(compiled.data[2].bytes, b"<blaze>\0");
    assert_eq!(compiled.data[3].relocs.targets(), [Target::Data(compiled.data[2].id)]);
}

#[test]
fn equal_data_is_one_object() {
    let mut requests = requests();
    let again = blob(b"inner\0\0\0", Vec::new(), Vec::new());
    requests.datas.push(DataRequest::Blob { blob: again, identity: DataIdentity::Shared });
    let compiled = convert(loading(&[1, 3, 4]), &requests).expect("converted");
    assert_eq!(compiled.data.len(), 4);
}

/// The data of requests nothing names is none of the function's: a fold's body and thunks
/// leave out what each other names.
#[test]
fn data_nothing_names_is_left_out() {
    let compiled = convert(loading(&[0]), &requests()).expect("converted");
    assert_eq!(compiled.data.len(), 1);
    assert_eq!(compiled.data[0].bytes, b"inner\0\0\0");
    assert!(convert(loading(&[]), &requests()).expect("converted").data.is_empty());
}

#[test]
fn a_thunk_calls_the_body() {
    let call = ModuleReloc {
        offset: 1,
        kind: binemit::Reloc::X86CallPCRel4,
        name: ModuleRelocTarget::User { namespace: crate::v2::fold::BODY, index: 0 },
        addend: -4,
    };
    let compiled = convert(code(vec![call], Vec::new()), &requests()).expect("converted");
    assert_eq!(compiled.relocs.targets(), [Target::Extern("body".to_owned())]);
}

#[test]
fn line_rows_take_their_spans() {
    let lines = vec![LineRow { offset: 0, span: 1 }, LineRow { offset: 4, span: 0 }];
    let compiled = convert(code(Vec::new(), lines), &requests()).expect("converted");
    let lines: Vec<_> = compiled.lines.iter().map(|it| (it.at, it.span)).collect();
    assert_eq!(lines, vec![(0, span(1)), (4, span(0))]);
    let past = vec![LineRow { offset: 0, span: 2 }];
    assert!(convert(code(Vec::new(), past), &requests()).is_err());
}

#[test]
fn what_v2_has_no_target_for_is_an_error() {
    let inlined = reloc(1, binemit::Reloc::X86CallPCRel4, crate::fragment::INLINED, 0);
    assert!(convert(code(vec![inlined], Vec::new()), &requests()).is_err());
    let mut writable = requests();
    writable.datas[0] = DataRequest::Blob {
        blob: blob(b"x", Vec::new(), Vec::new()),
        identity: DataIdentity::Writable,
    };
    assert!(convert(loading(&[0]), &writable).is_err());
}

/// A static's value that points at a writable object and at an equal read-only one: the
/// writable one is the static's own, as rustc's global of a nested mutable allocation is,
/// so two statics' are two objects where their read-only ones are one.
#[test]
fn a_statics_writable_data_is_its_own() {
    let bytes = b"counter\0";
    let requests = Requests {
        funcs: Vec::new(),
        datas: vec![
            DataRequest::Blob { blob: blob(bytes, Vec::new(), Vec::new()), identity: DataIdentity::Writable },
            DataRequest::Blob { blob: blob(bytes, Vec::new(), Vec::new()), identity: DataIdentity::Shared },
        ],
    };
    let init = blob(&[0; 16], vec![(0, DataId::from_u32(0), 0), (8, DataId::from_u32(1), 0)], Vec::new());
    let func = |_: u32, _: &FuncRequest| Ok(callee());
    let data = |_: u32, _: &DataRequest| Ok(DataTarget::Own);
    let symbol = |name: &str| Ok(Target::Extern(name.to_owned()));
    let body = || Ok(Target::Extern("body".to_owned()));
    let resolve = Resolve { func: &func, data: &data, symbol: &symbol, body: &body };
    let of = |owner: &[u8]| {
        let owner = ContentId::of("test", owner);
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            value(frame, Arch::X86_64, &init, &requests, &owner, &resolve)
        })
        .expect("converted")
    };
    let ((one, one_relocs), (two, _)) = (of(b"one"), of(b"two"));
    assert_eq!(one.iter().map(|it| it.writable).collect::<Vec<_>>(), [true, false]);
    assert!(one.iter().all(|it| it.bytes == bytes));
    assert_ne!(one[0].id, two[0].id, "two statics' writable objects are two");
    assert_eq!(one[1].id, two[1].id, "equal read-only data is one object");
    assert_eq!(one_relocs.targets(), [Target::Data(one[0].id), Target::Data(one[1].id)]);
}

/// A relocation at one of the function's SanitizerCoverage arrays is at its own array, with
/// its addend into it; there are three.
#[cfg(feature = "llvm")]
#[test]
fn a_coverage_relocation_is_at_the_functions_own_array() {
    let namespace = crate::llvm::sanitize::COVERAGE;
    let at = |array| reloc(1 + 8 * array, binemit::Reloc::X86PCRel4, namespace, array);
    let compiled = convert(code((0..3).map(at).collect(), Vec::new()), &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| (it.target.clone(), it.addend)).collect();
    let own = |array| (Target::Coverage(array), -4);
    assert_eq!(targets, vec![own(0), own(1), own(2)]);
    assert_eq!(compiled.coverage, None);
    let past = reloc(1, binemit::Reloc::X86PCRel4, namespace, 3);
    assert!(convert(code(vec![past], Vec::new()), &requests()).is_err());
}

/// A relocation at the tier counter cells is at the instance's own (`Target::Counter` of its
/// key), the cell by its addend, whether stencil glue or counting Cranelift code wrote it.
#[test]
fn a_counter_relocation_is_at_the_instances_own_cells() {
    let namespace = blaze_stencil::tier::COUNTER_NAMESPACE;
    let mut calls = reloc(3, binemit::Reloc::X86PCRel4, namespace, 0);
    calls.addend = -5;
    let mut back_edges = reloc(11, binemit::Reloc::X86PCRel4, namespace, 0);
    back_edges.addend = 3;
    let compiled = convert(code(vec![calls, back_edges], Vec::new()), &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| (it.target.clone(), it.addend)).collect();
    assert_eq!(targets, vec![(Target::Counter(key()), -5), (Target::Counter(key()), 3)]);
    let other = reloc(3, binemit::Reloc::X86PCRel4, namespace, 1);
    assert!(convert(code(vec![other], Vec::new()), &requests()).is_err());
}

/// A relocation into the function's own code (stencil code's `Name::Own`, Cranelift's
/// `FunctionOffset`) is at its own instance, `Target::Direct` of its key, the offset added to
/// its addend; an offset past the code's end is refused.
#[test]
fn a_relocation_into_its_own_code_is_at_the_instance() {
    let into = |at, offset, addend| ModuleReloc {
        offset: at,
        kind: binemit::Reloc::X86PCRel4,
        name: ModuleRelocTarget::FunctionOffset(FuncId::from_u32(0), offset),
        addend,
    };
    let relocs = vec![into(1, 8, -4), into(9, 0, -4), into(17, 32, 0)];
    let compiled = convert(code(relocs, Vec::new()), &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| (it.target.clone(), it.addend)).collect();
    let own = |addend| (Target::Direct(key()), addend);
    assert_eq!(targets, vec![own(4), own(-4), own(32)]);
    assert_eq!(compiled.relocs.targets(), [Target::Direct(key())]);
    assert!(convert(code(vec![into(1, 33, 0)], Vec::new()), &requests()).is_err());
    assert!(convert(code(vec![into(1, 8, i64::MAX)], Vec::new()), &requests()).is_err());
}

/// An instrumented function's PGO counters are its own, the counter by the addend.
#[cfg(feature = "llvm")]
#[test]
fn a_pgo_counter_relocation_is_at_the_functions_own_counters() {
    let namespace = crate::llvm::pgo::COUNTER_RELOCS;
    let mut second = reloc(3, binemit::Reloc::X86PCRel4, namespace, 0);
    second.addend = 4;
    let compiled = convert(code(vec![second], Vec::new()), &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| (it.target.clone(), it.addend)).collect();
    assert_eq!(targets, vec![(Target::ProfileCounters, 4)]);
    assert_eq!(compiled.profile, None);
    let other = reloc(3, binemit::Reloc::X86PCRel4, namespace, 1);
    assert!(convert(code(vec![other], Vec::new()), &requests()).is_err());
}

#[test]
fn relocation_kinds() {
    use binemit::Reloc as R;
    let kinds = [
        (R::Abs8, RelocKind::Abs64),
        (R::X86PCRel4, RelocKind::PcRel32),
        (R::X86CallPCRel4, RelocKind::PcRel32),
        (R::X86CallPLTRel4, RelocKind::PcRel32),
        (R::X86GOTPCRel4, RelocKind::GotPcRel32),
        (R::Arm64Call, RelocKind::Call26),
        (R::Aarch64AdrPrelPgHi21, RelocKind::AdrpPage21),
        (R::Aarch64AddAbsLo12Nc, RelocKind::PageOffset12),
        (R::Aarch64AdrGotPage21, RelocKind::GotPage21),
        (R::Aarch64Ld64GotLo12Nc, RelocKind::GotPageOffset12),
        (R::ElfX86_64TlsGd, RelocKind::TlsGd32),
        (R::MachOX86_64Tlv, RelocKind::TlvRel32),
        (R::MachOAarch64TlsAdrPage21, RelocKind::TlvPage21),
        (R::MachOAarch64TlsAdrPageOff12, RelocKind::TlvPageOffset12),
        (R::Aarch64TlsDescAdrPage21, RelocKind::TlsDescPage21),
        (R::Aarch64TlsDescLd64Lo12, RelocKind::TlsDescLd64Lo12),
        (R::Aarch64TlsDescAddLo12, RelocKind::TlsDescAddLo12),
        (R::Aarch64TlsDescCall, RelocKind::TlsDescCall),
        (R::X86SecRel, RelocKind::SectionRel32),
    ];
    for (cranelift, v2) in kinds {
        assert_eq!(super::reloc_kind(cranelift), Ok(v2));
    }
    assert!(super::reloc_kind(R::Abs4).is_err());
    assert_eq!(super::elf_kind(283), Ok(RelocKind::Call26));
    // The linker reads an AArch64 LO12's scale from the instruction.
    for elf in [277, 278, 284, 285, 286, 299] {
        assert_eq!(super::elf_kind(elf), Ok(RelocKind::PageOffset12), "ELF type {elf}");
    }
    let tls = [
        (19, RelocKind::TlsGd32),
        (22, RelocKind::TlsGotTpOff32),
        (23, RelocKind::TlsOffset),
        (541, RelocKind::TlsIePage21),
        (542, RelocKind::TlsIeLo12),
        (549, RelocKind::TpHi12),
        (551, RelocKind::TpLo12),
        (569, RelocKind::TlsDescCall),
    ];
    for (elf, v2) in tls {
        assert_eq!(super::elf_kind(elf), Ok(v2), "ELF type {elf}");
    }
    // R_AARCH64_TLSLE_ADD_TPREL_LO12, which checks for overflow that `TpLo12` does not.
    assert!(super::elf_kind(550).is_err());
}
