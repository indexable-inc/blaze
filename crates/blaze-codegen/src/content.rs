//! Content IDs of what codegen makes: a data object's (what [`Target::Data`] names, so equal
//! bytes are one object) and a compiled function's code (what code dedupe compares).

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};

use crate::{Arch, CompiledFunction, Covered, Data, Line, Profiled, RelocKind, Relocs, Target, Unwind};

pub const DATA_DOMAIN: &str = "blaze v2 data 3";
pub const CODE_DOMAIN: &str = "blaze v2 code 4";
pub const OWNED_DATA_DOMAIN: &str = "blaze v2 owned data 1";

/// The content ID of a read-only data object: its alignment, bytes and relocations. Two
/// objects with one ID are one object.
pub fn data_id(align: u32, bytes: &[u8], relocs: &Relocs) -> ContentId {
    let mut hasher = Hasher::new(DATA_DOMAIN);
    hasher.update(&align.to_le_bytes()).part(bytes);
    relocs_into(&mut hasher, relocs, target_into);
    hasher.finish()
}

impl Data {
    /// A data object, its ID made from what it is.
    pub fn new(align: u32, bytes: Vec<u8>, relocs: Relocs) -> Data {
        Data { id: data_id(align, &bytes, &relocs), align, bytes, relocs, writable: false }
    }

    /// The `index`th writable object the static `owner` (its item key) points to: one object
    /// whatever another static's holds, so two statics' equal bytes are two addresses, and
    /// another object once its bytes change.
    pub fn owned(owner: &ContentId, index: u32, align: u32, bytes: Vec<u8>, relocs: Relocs) -> Data {
        let mut hasher = Hasher::new(OWNED_DATA_DOMAIN);
        hasher.id(owner).update(&index.to_le_bytes()).id(&data_id(align, &bytes, &relocs));
        Data { id: hasher.finish(), align, bytes, relocs, writable: true }
    }
}

/// The content ID of what a compiled function does: its architecture, alignment, bytes,
/// relocations, data, unwind information, lines, coverage and profile. Not its key, what it inlined or its side
/// table: two functions with one code ID run the same instructions against the same
/// addresses, unwind alike and show the same lines in a backtrace.
pub fn code_id(function: &CompiledFunction) -> ContentId {
    let mut hasher = Hasher::new(CODE_DOMAIN);
    hasher.update(&[arch_tag(function.arch)]);
    hasher.update(&function.align.to_le_bytes()).part(&function.code);
    relocs_into(&mut hasher, &function.relocs, target_into);
    hasher.update(&(function.data.len() as u64).to_le_bytes());
    for data in &function.data {
        hasher.id(&data.id);
    }
    match &function.unwind {
        None => hasher.update(&[0]),
        Some(Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
            hasher.update(&[1]).update(&code_align.to_le_bytes()).update(&data_align.to_le_bytes());
            hasher.update(&return_address.to_le_bytes()).part(cie).part(fde);
            optional_target_into(&mut hasher, personality.as_ref());
            optional_id_into(&mut hasher, lsda.as_ref())
        }
        Some(Unwind::Windows { info, handler, lsda }) => {
            hasher.update(&[2]).part(info);
            optional_target_into(&mut hasher, handler.as_ref());
            optional_id_into(&mut hasher, lsda.as_ref())
        }
    };
    lines_into(&mut hasher, &function.lines);
    covered_into(&mut hasher, function.coverage.as_ref());
    profiled_into(&mut hasher, function.profile.as_ref());
    hasher.finish()
}

/// A function's SanitizerCoverage arrays and PC table: two functions that count other blocks
/// are other code.
pub(crate) fn covered_into(hasher: &mut Hasher, covered: Option<&Covered>) {
    let Some(Covered { counters, flags, guards, pcs }) = covered else {
        hasher.update(&[0]);
        return;
    };
    hasher.update(&[1]).update(&counters.to_le_bytes()).update(&flags.to_le_bytes());
    hasher.update(&guards.to_le_bytes()).update(&(pcs.len() as u64).to_le_bytes());
    for (at, pc_flags) in pcs {
        hasher.update(&at.to_le_bytes()).update(&pc_flags.to_le_bytes());
    }
}

/// What PGO made of a function: an instrumented one counts under its own name and record, and
/// one built with a profile is other code than one built without.
pub(crate) fn profiled_into(hasher: &mut Hasher, profiled: Option<&Profiled>) {
    match profiled {
        None => hasher.update(&[0]),
        Some(Profiled::Counted(record)) => {
            hasher.update(&[1]).update(&record.name).update(&record.name_ref.to_le_bytes());
            hasher.update(&record.hash.to_le_bytes()).update(&record.counters.to_le_bytes());
            hasher.update(&record.version.to_le_bytes())
        }
        Some(Profiled::Used { mismatched }) => hasher.update(&[2, u8::from(*mismatched)]),
    };
}

fn optional_target_into<'h>(hasher: &'h mut Hasher, target: Option<&Target>) -> &'h mut Hasher {
    match target {
        Some(target) => {
            hasher.update(&[1]);
            target_into(hasher, target);
            hasher
        }
        None => hasher.update(&[0]),
    }
}

fn optional_id_into<'h>(hasher: &'h mut Hasher, id: Option<&ContentId>) -> &'h mut Hasher {
    match id {
        Some(id) => hasher.update(&[1]).id(id),
        None => hasher.update(&[0]),
    }
}

pub(crate) fn lines_into(hasher: &mut Hasher, lines: &[Line]) {
    hasher.update(&(lines.len() as u64).to_le_bytes());
    for Line { at, span: SpanRef { anchor, index } } in lines {
        hasher.update(&at.to_le_bytes()).id(anchor).update(&index.to_le_bytes());
    }
}

/// A table as it is kept: its rows, each distinct target once, its wide addends. A table has
/// one form for what it holds, so this is a function of its relocations.
pub(crate) fn relocs_into<T>(hasher: &mut Hasher, relocs: &Relocs<T>, mut target_into: impl FnMut(&mut Hasher, &T)) {
    hasher.update(&(relocs.len() as u64).to_le_bytes());
    // 64 rows an update: BLAKE3 runs at its streaming rate on whole kilobytes.
    let mut buffer = [0u8; 64 * 16];
    for rows in relocs.rows().chunks(64) {
        for (row, out) in rows.iter().zip(buffer.chunks_exact_mut(16)) {
            out.copy_from_slice(&row.to_le_bytes());
        }
        hasher.update(&buffer[..16 * rows.len()]);
    }
    hasher.update(&(relocs.targets().len() as u64).to_le_bytes());
    for target in relocs.targets() {
        target_into(hasher, target);
    }
    hasher.update(&(relocs.wide().len() as u64).to_le_bytes());
    for (row, addend) in relocs.wide() {
        hasher.update(&row.to_le_bytes()).update(&addend.to_le_bytes());
    }
}

/// A relocation target into `hasher`: a tag byte per kind, then what names it.
pub fn target_into(hasher: &mut Hasher, target: &Target) {
    match target {
        Target::Function(key) => hasher.update(&[0]).id(&key.0),
        Target::Direct(key) => hasher.update(&[1]).id(&key.0),
        Target::Static { item, instance } => hasher.update(&[2]).id(&item.0).id(instance),
        Target::Data(id) => hasher.update(&[3]).id(id),
        Target::Extern(name) => hasher.update(&[4]).part(name.as_bytes()),
        Target::Position(SpanRef { anchor, index }) => {
            hasher.update(&[5]).id(anchor).update(&index.to_le_bytes())
        }
        Target::Weak(name) => hasher.update(&[6]).part(name.as_bytes()),
        Target::Coverage(array) => hasher.update(&[7]).update(&array.to_le_bytes()),
        Target::Counter(key) => hasher.update(&[8]).id(&key.0),
        Target::ProfileCounters => hasher.update(&[9]),
    };
}

pub const TARGET_DOMAIN: &str = "blaze v2 target 3";
/// A [`Target::Counter`]'s own domain: its cells are its instance's, by the key alone.
pub const COUNTER_DOMAIN: &str = "blaze v2 counter 1";

/// A relocation target's content ID, to set targets apart without comparing them.
pub fn target_id(target: &Target) -> ContentId {
    if let Target::Counter(key) = target {
        let mut hasher = Hasher::new(COUNTER_DOMAIN);
        hasher.id(&key.0);
        return hasher.finish();
    }
    let mut hasher = Hasher::new(TARGET_DOMAIN);
    target_into(&mut hasher, target);
    hasher.finish()
}

/// The byte an [`Arch`] is hashed and stored as.
pub fn arch_tag(arch: Arch) -> u8 {
    match arch {
        Arch::X86_64 => 0,
        Arch::Aarch64 => 1,
        Arch::Wasm32 => 2,
    }
}

/// The [`Arch`] of a byte [`arch_tag`] gave; `None` for another byte.
pub fn arch_of(tag: u8) -> Option<Arch> {
    Some(match tag {
        0 => Arch::X86_64,
        1 => Arch::Aarch64,
        2 => Arch::Wasm32,
        _ => return None,
    })
}

/// Every kind, by its tag ([`kind_tag`]); a new kind goes at the end.
const KINDS: [RelocKind; 26] = [
    RelocKind::Abs64,
    RelocKind::PcRel32,
    RelocKind::GotPcRel32,
    RelocKind::Call26,
    RelocKind::AdrpPage21,
    RelocKind::PageOffset12,
    RelocKind::GotPage21,
    RelocKind::GotPageOffset12,
    RelocKind::TlsOffset,
    RelocKind::WasmFunction,
    RelocKind::WasmTable,
    RelocKind::WasmMemory,
    RelocKind::TlsGd32,
    RelocKind::TlsGotTpOff32,
    RelocKind::TlvRel32,
    RelocKind::TlvPage21,
    RelocKind::TlvPageOffset12,
    RelocKind::TlsDescPage21,
    RelocKind::TlsDescLd64Lo12,
    RelocKind::TlsDescAddLo12,
    RelocKind::TlsDescCall,
    RelocKind::TlsIePage21,
    RelocKind::TlsIeLo12,
    RelocKind::TpHi12,
    RelocKind::TpLo12,
    RelocKind::SectionRel32,
];

/// The byte a [`RelocKind`] hashes as in a content key, and in blaze-premap's.
pub fn kind_tag(kind: RelocKind) -> u8 {
    match kind {
        RelocKind::Abs64 => 0,
        RelocKind::PcRel32 => 1,
        RelocKind::GotPcRel32 => 2,
        RelocKind::Call26 => 3,
        RelocKind::AdrpPage21 => 4,
        RelocKind::PageOffset12 => 5,
        RelocKind::GotPage21 => 6,
        RelocKind::GotPageOffset12 => 7,
        RelocKind::TlsOffset => 8,
        RelocKind::WasmFunction => 9,
        RelocKind::WasmTable => 10,
        RelocKind::WasmMemory => 11,
        RelocKind::TlsGd32 => 12,
        RelocKind::TlsGotTpOff32 => 13,
        RelocKind::TlvRel32 => 14,
        RelocKind::TlvPage21 => 15,
        RelocKind::TlvPageOffset12 => 16,
        RelocKind::TlsDescPage21 => 17,
        RelocKind::TlsDescLd64Lo12 => 18,
        RelocKind::TlsDescAddLo12 => 19,
        RelocKind::TlsDescCall => 20,
        RelocKind::TlsIePage21 => 21,
        RelocKind::TlsIeLo12 => 22,
        RelocKind::TpHi12 => 23,
        RelocKind::TpLo12 => 24,
        RelocKind::SectionRel32 => 25,
    }
}

pub(crate) fn kind_of(tag: u8) -> Option<RelocKind> {
    KINDS.get(tag as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceKey, Relocation};

    fn function(code: &[u8], relocs: Vec<Relocation>) -> CompiledFunction {
        CompiledFunction {
            key: InstanceKey(ContentId([7; 32])),
            arch: Arch::X86_64,
            align: 16,
            code: code.to_vec(),
            relocs: relocs.into(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    #[test]
    fn unwinding_and_lines_are_the_code() {
        let a = function(&[0xc3], Vec::new());
        let mut b = a.clone();
        b.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8],
            fde: Vec::new(),
            personality: None,
            lsda: None,
        });
        let mut c = a.clone();
        c.lines = vec![Line { at: 0, span: SpanRef { anchor: ContentId([1; 32]), index: 0 } }];
        assert_ne!(code_id(&a), code_id(&b));
        assert_ne!(code_id(&a), code_id(&c));
    }

    #[test]
    fn what_a_function_counts_is_its_code() {
        let a = function(&[0xc3], Vec::new());
        let covered = |pcs| CompiledFunction {
            coverage: Some(Covered { counters: 1, flags: 0, guards: 0, pcs }),
            ..a.clone()
        };
        let (b, c) = (covered(vec![(0, 1)]), covered(vec![(0, 0)]));
        assert_ne!(code_id(&a), code_id(&b));
        assert_ne!(code_id(&b), code_id(&c));
        assert_ne!(target_id(&Target::Coverage(0)), target_id(&Target::Coverage(1)));
    }

    /// An instrumented function's record, and whether a profile applied, are its code.
    #[test]
    fn what_pgo_made_of_a_function_is_its_code() {
        let a = function(&[0xc3], Vec::new());
        let record = crate::ProfileRecord { name: [3; 16], name_ref: 5, hash: 7, counters: 2, version: 10 };
        let profiled = |profile| CompiledFunction { profile: Some(profile), ..a.clone() };
        let counted = profiled(Profiled::Counted(record));
        let ids = [
            code_id(&a),
            code_id(&counted),
            code_id(&profiled(Profiled::Counted(crate::ProfileRecord { hash: 8, ..record }))),
            code_id(&profiled(Profiled::Counted(crate::ProfileRecord { name: [4; 16], ..record }))),
            code_id(&profiled(Profiled::Counted(crate::ProfileRecord { counters: 3, ..record }))),
            code_id(&profiled(Profiled::Used { mismatched: false })),
            code_id(&profiled(Profiled::Used { mismatched: true })),
        ];
        let distinct: std::collections::BTreeSet<ContentId> = ids.iter().copied().collect();
        assert_eq!(distinct.len(), ids.len());
        assert_eq!(code_id(&counted), code_id(&counted.clone()));
    }

    /// Every target, one of each variant: a new variant gets a line here.
    fn every_target() -> Vec<Target> {
        let id = ContentId([9; 32]);
        vec![
            Target::Function(InstanceKey(id)),
            Target::Direct(InstanceKey(id)),
            Target::Static { item: blaze_items::ItemKey(id), instance: id },
            Target::Data(id),
            Target::Extern("x".to_owned()),
            Target::Weak("x".to_owned()),
            Target::Position(SpanRef { anchor: id, index: 0 }),
            Target::Coverage(0),
            Target::Counter(InstanceKey(id)),
            Target::ProfileCounters,
        ]
    }

    #[test]
    fn every_target_has_its_own_id() {
        let targets = every_target();
        for target in &targets {
            // Exhaustive: a new variant is a compile error here until `every_target` has it.
            match target {
                Target::Function(_)
                | Target::Direct(_)
                | Target::Static { .. }
                | Target::Data(_)
                | Target::Extern(_)
                | Target::Weak(_)
                | Target::Position(_)
                | Target::Coverage(_)
                | Target::Counter(_)
                | Target::ProfileCounters => {}
            }
        }
        let ids: std::collections::BTreeSet<ContentId> = targets.iter().map(target_id).collect();
        assert_eq!(ids.len(), targets.len());
    }

    /// A counter is its instance's: by the key, in a domain of its own, and two instances'
    /// cells are other targets, so their code is other code.
    #[test]
    fn a_counter_is_its_instances_own() {
        let (a, b) = (InstanceKey(ContentId([1; 32])), InstanceKey(ContentId([2; 32])));
        let mut hasher = Hasher::new(COUNTER_DOMAIN);
        hasher.id(&a.0);
        assert_eq!(target_id(&Target::Counter(a)), hasher.finish());
        assert_ne!(target_id(&Target::Counter(a)), target_id(&Target::Counter(b)));
        assert_ne!(target_id(&Target::Counter(a)), target_id(&Target::Function(a)));
        let add = [0x48, 0x83, 0x05, 0, 0, 0, 0, 1];
        let counting = |key: InstanceKey| {
            let reloc =
                Relocation { at: 3, kind: RelocKind::PcRel32, target: Target::Counter(key), addend: 4 };
            CompiledFunction { key, ..function(&add, vec![reloc]) }
        };
        assert_ne!(code_id(&counting(a)), code_id(&counting(b)));
        assert_ne!(code_id(&counting(a)), code_id(&function(&add, Vec::new())));
    }

    #[test]
    fn every_kind_has_its_own_tag() {
        for tag in 0..KINDS.len() as u8 {
            assert_eq!(kind_tag(kind_of(tag).unwrap()), tag);
        }
        assert_eq!(kind_of(KINDS.len() as u8), None);
        let mut tags: Vec<u8> = KINDS.iter().map(|it| kind_tag(*it)).collect();
        tags.dedup();
        assert_eq!(tags.len(), KINDS.len());
    }

    #[test]
    fn the_key_is_not_the_code() {
        let a = function(&[0xc3], Vec::new());
        let mut b = a.clone();
        b.key = InstanceKey(ContentId([8; 32]));
        assert_eq!(code_id(&a), code_id(&b));
    }

    #[test]
    fn a_relocation_target_is_the_code() {
        let reloc = |n, addend| Relocation {
            at: 1,
            kind: RelocKind::PcRel32,
            target: Target::Function(InstanceKey(ContentId([n; 32]))),
            addend,
        };
        let a = function(&[0xe8, 0, 0, 0, 0], vec![reloc(1, -4)]);
        let b = function(&[0xe8, 0, 0, 0, 0], vec![reloc(2, -4)]);
        assert_ne!(code_id(&a), code_id(&b));
        let wide = function(&[0xe8, 0, 0, 0, 0], vec![reloc(1, 1 << 40)]);
        let wider = function(&[0xe8, 0, 0, 0, 0], vec![reloc(1, 1 << 41)]);
        assert_ne!(code_id(&a), code_id(&wide));
        assert_ne!(code_id(&wide), code_id(&wider));
    }

    /// blaze-premap's image key hashes names' targets with [`target_into`] too: each kind,
    /// even of the same name or ID, is a different input.
    #[test]
    fn every_kind_of_target_hashes_apart() {
        let id = ContentId([1; 32]);
        let key = InstanceKey(id);
        let targets = [
            Target::Function(key),
            Target::Direct(key),
            Target::Static { item: blaze_items::ItemKey(id), instance: id },
            Target::Data(id),
            Target::Extern("a".into()),
            Target::Weak("a".into()),
            Target::Position(SpanRef { anchor: id, index: 1 }),
            Target::Coverage(1),
            Target::Counter(key),
            Target::ProfileCounters,
        ];
        let mut ids: Vec<ContentId> = targets.iter().map(target_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), targets.len());
    }

    #[test]
    fn data_ids_frame_their_parts() {
        let none = Relocs::new();
        assert_ne!(data_id(1, b"ab", &none), data_id(2, b"ab", &none));
        assert_ne!(data_id(1, b"ab", &none), data_id(1, b"abc", &none));
    }

    /// A static's writable object is its own: two statics' equal bytes are two objects, as are
    /// two of one static's, neither is the read-only object of the same bytes, and new bytes
    /// are a new object.
    #[test]
    fn writable_data_is_its_statics_own() {
        let (one, two) = (ContentId([1; 32]), ContentId([2; 32]));
        let owned = |owner: &ContentId, index, bytes: &[u8]| Data::owned(owner, index, 4, bytes.to_vec(), Relocs::new());
        let shared = Data::new(4, vec![0; 4], Relocs::new());
        assert!(owned(&one, 0, &[0; 4]).writable && !shared.writable);
        let ids = [
            owned(&one, 0, &[0; 4]).id,
            owned(&two, 0, &[0; 4]).id,
            owned(&one, 1, &[0; 4]).id,
            owned(&one, 0, &[1; 4]).id,
            shared.id,
        ];
        let mut distinct = ids.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), ids.len(), "{ids:?}");
        assert_eq!(owned(&one, 0, &[0; 4]), owned(&one, 0, &[0; 4]));
    }
}
