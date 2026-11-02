//! A compiled function's template, a static's and a function's masked shape as the store keeps
//! them (`crates/blaze-codegen/src/blob.rs`, `blaze_cas::Kind::Code`): little-endian, every
//! length before what it counts, a magic each (`BZT5`, `BZS3`, `BZM1`). Read by `decode`,
//! `decode_view`, `decode_static` and `decode_masked`, written by `encode`, `encode_static` and
//! `encode_masked`. The bytes come from the store or the remote cache.
//!
//! Every input goes through each of the four decoders. Invariants, each checked below (a
//! violation panics, which the engine reports as a crash):
//! - no decoder panics or hangs: bytes that are not a blob are a `BlobError`;
//! - round trip: a value read, written again and read gives the same value (the format need
//!   not be canonical, so the bytes written need not be the input);
//! - `decode_view` in a fresh frame agrees with `decode`: the same error, or a view whose owned
//!   template is the one `decode` gives.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::{
    Arch, Covered, Line, ProfileRecord, Profiled, RelocKind, Relocation, Relocs,
    blob::{decode, decode_masked, decode_static, decode_view, encode, encode_masked, encode_static},
    fold::{Constant, Masked},
    template::{Ref, StaticTemplate, Template, TemplateData, TemplateInit, TemplateUnwind, TemplateView},
    wasm::{Import, Signature, ValType},
};
use blaze_format::SpanRef;
use blaze_fuzzer::Target;
use blaze_ids::ContentId;
use blaze_mem::{Tag, frame};

const TARGET: Target = Target {
    name: "codegen_blob",
    check,
    seeds,
    dictionary: &[
        b"BZT5",
        b"BZS3",
        b"BZM1",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
        b"\x10\x00\x00\x00",
        b"\xff\xff\xff\xff",
    ],
    max_len: 1 << 14,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    let decoded = decode(data);
    let viewed = frame::task(Tag::Codegen, |frame| decode_view(data, frame).map(TemplateView::owned));
    assert!(viewed == decoded, "decode_view: {viewed:?} where decode gives {decoded:?}");
    if let Ok(template) = decoded {
        let again = decode(&encode(&template));
        assert!(again.as_ref() == Ok(&template), "round trip: a template read and written reads as {again:?}");
    }
    if let Ok(template) = decode_static(data) {
        let again = decode_static(&encode_static(&template));
        assert!(again.as_ref() == Ok(&template), "round trip: a static read and written reads as {again:?}");
    }
    if let Ok(masked) = decode_masked(data) {
        let again = decode_masked(&encode_masked(masked.as_ref()));
        assert!(again.as_ref() == Ok(&masked), "round trip: a masked shape read and written reads as {again:?}");
    }
}

fn relocation(target: Ref, addend: i64) -> Relocation<Ref> {
    Relocation { at: 3, kind: RelocKind::Call26, target, addend }
}

/// The least template: one `ret`, nothing else.
fn bare() -> Template {
    Template {
        arch: Arch::X86_64,
        align: 16,
        code: vec![0xc3],
        relocs: Relocs::new(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        refs: 0,
        externs: Vec::new(),
        coverage: None,
        profile: None,
    }
}

/// A template with every kind of reference, data, DWARF unwinding, lines, an outside symbol,
/// coverage and a profile record.
fn full() -> Template {
    Template {
        arch: Arch::Aarch64,
        align: 4,
        code: vec![1, 2, 3, 4, 5, 6, 7, 8],
        relocs: vec![
            relocation(Ref::Place(2), -8),
            relocation(Ref::Data(0), 1 << 40),
            relocation(Ref::Position(SpanRef { anchor: ContentId([4; 32]), index: 9 }), -8),
            relocation(Ref::Extern(0), -8),
            relocation(Ref::Coverage(2), i64::MIN),
            relocation(Ref::Counter, 8),
            relocation(Ref::ProfileCounters, 16),
            relocation(Ref::Own, 24),
            relocation(Ref::Place(2), 0),
        ]
        .into(),
        data: vec![TemplateData { align: 8, bytes: vec![9; 8], relocs: vec![relocation(Ref::Place(0), -8)].into(), writable: false }],
        inlined: Vec::new(),
        side: Some(ContentId([6; 32])),
        unwind: Some(TemplateUnwind::Dwarf {
            code_align: 4,
            data_align: -8,
            return_address: 30,
            cie: vec![0x0c, 31, 0],
            fde: vec![0x44, 0x0e, 16],
            personality: Some(1),
            lsda: Some(0),
        }),
        lines: vec![Line { at: 4, span: SpanRef { anchor: ContentId([7; 32]), index: 3 } }],
        refs: 3,
        externs: vec!["bcmp".to_owned()],
        coverage: Some(Covered { counters: 2, flags: 0, guards: 8, pcs: vec![(0, 1), (4, 0)] }),
        profile: Some(Profiled::Counted(ProfileRecord {
            name: [8; 16],
            name_ref: 1 << 60,
            hash: 3,
            counters: 5,
            version: 10,
        })),
    }
}

fn statics() -> [StaticTemplate; 2] {
    let reloc = Relocation { at: 8, kind: RelocKind::Abs64, target: Ref::Data(0), addend: 0 };
    let bytes = StaticTemplate {
        align: 16,
        init: TemplateInit::Bytes { bytes: vec![0; 24], relocs: vec![reloc].into() },
        data: vec![TemplateData { align: 1, bytes: b"hi".to_vec(), relocs: Relocs::new(), writable: false }],
        mutable: true,
        thread_local: true,
        section: Some("__DATA,__mod_init_func".to_owned()),
        imports: vec![(
            "fd_write".to_owned(),
            Import {
                module: "wasi_snapshot_preview1".to_owned(),
                field: "fd_write".to_owned(),
                ty: Signature { params: vec![ValType::I32; 4], results: vec![ValType::I32] },
            },
        )],
        refs: 0,
    };
    let zeroed = StaticTemplate {
        align: 8,
        init: TemplateInit::Zeroed { size: 1 << 30 },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
        refs: 0,
    };
    [bytes, zeroed]
}

fn masked() -> Masked {
    Masked {
        shape: vec![1, 2, 3],
        constants: vec![
            Constant::Int { bits: 64, value: 24 },
            Constant::F32(0x3f80_0000),
            Constant::F64(1),
            Constant::Place(4),
            Constant::Data(ContentId([9; 32])),
        ],
        size: 40,
    }
}

/// What the writers make of templates, statics and masked shapes of every shape.
fn valid() -> Vec<Vec<u8>> {
    let mut windows = full();
    windows.unwind = Some(TemplateUnwind::Windows { info: vec![1, 2], handler: None, lsda: Some(0) });
    windows.profile = Some(Profiled::Used { mismatched: true });
    let mut out = vec![encode(&bare()), encode(&full()), encode(&windows)];
    out.extend(statics().iter().map(encode_static));
    out.push(encode_masked(Some(&masked())));
    out.push(encode_masked(None));
    out
}

/// The valid blobs, and each a byte short and a byte long.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = valid();
    for bytes in valid() {
        out.push(bytes[..bytes.len() - 1].to_vec());
        out.push([&bytes[..], &[0]].concat());
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::blob::{decode, decode_masked, decode_static};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    /// The writers' blobs are read by their decoders, so the seeds reach past every magic.
    #[test]
    fn valid_blobs_read_back() {
        for bytes in super::valid() {
            assert!(
                decode(&bytes).is_ok() || decode_static(&bytes).is_ok() || decode_masked(&bytes).is_ok(),
                "a blob the writers made reads as none of them"
            );
        }
    }
}
