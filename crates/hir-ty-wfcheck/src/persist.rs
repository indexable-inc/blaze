//! [`WfResult`] in blaze's format: one row per diagnostic, its site as a tag word and the query
//! database id of the definition it is in (the database that wrote a buffer is the one reading it, as
//! `crates/hir-def/src/hir/persist.rs` writes ids), its message in the buffer.

use std::mem::offset_of;

use blaze_format::{FormatError, Persist, Record, SchemaHash, Slice, Str, Writer, open_as};
use hir_def::{
    AdtId, ConstId, EnumId, EnumVariantId, FunctionId, GenericDefId, ImplId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId, VariantId,
};
use la_arena::{Idx, RawIdx};
use blaze_query::db::{AsId, FromId};

use crate::{Site, WfDiagnostic, WfResult};

blaze_format::record! {
    /// One diagnostic. `kind` holds the site's variant in its low byte, the kind of the
    /// definition `id` names in the next, and the error code in the third.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct WfRow: 1 {
        pub id: u64,
        pub message: Str,
        pub kind: u32,
        pub index: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct WfRoot: 1 {
        rows: Slice<WfRow>,
    }
}

const _: () = assert!(size_of::<WfRow>() == 24);

/// The codes a check reports, by their number in a row.
const CODES: [&str; 4] = ["E0309", "E0310", "E0311", "E0477"];

/// A [`WfResult`] read in place.
#[derive(Clone, Copy, Debug)]
pub struct WfResultRef<'a> {
    buffer: &'a [u8],
    rows: &'a [WfRow],
}

/// One diagnostic read in place: its message borrows the buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WfDiagnosticRef<'a> {
    pub code: &'static str,
    pub message: &'a str,
    pub site: Site,
}

impl<'a> WfResultRef<'a> {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The diagnostics in place, in order.
    pub fn iter(&self) -> impl Iterator<Item = Result<WfDiagnosticRef<'a>, FormatError>> + 'a {
        let buffer = self.buffer;
        self.rows.iter().map(move |row| decode(row, buffer))
    }

    /// The result as the query returns it.
    pub fn to_result(&self) -> Result<WfResult, FormatError> {
        let mut diagnostics = Vec::with_capacity(self.rows.len());
        for it in self.iter() {
            let WfDiagnosticRef { code, message, site } = it?;
            diagnostics.push(WfDiagnostic { code, message: message.to_owned(), site }); // alloc: the edge, a memo read back from the store
        }
        Ok(WfResult { diagnostics })
    }
}

impl Persist for WfResult {
    const SCHEMA: SchemaHash = blaze_format::schema_of(&[
        blaze_format::Part::Str("WfResult"),
        blaze_format::Part::U64(1),
        blaze_format::Part::Schema(WfRoot::SCHEMA),
    ]);
    type Ref<'a> = WfResultRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let text: usize = self.diagnostics.iter().map(|it| it.message.len()).sum();
        out.bytes.reserve(text + (self.diagnostics.len() + 1) * size_of::<WfRow>());
        let messages: Vec<u64> =
            self.diagnostics.iter().map(|it| out.push_str(&it.message)).collect(); // alloc: the memo's persisted form, once per write
        let rows_at = out.next::<WfRow>();
        let rows: Vec<WfRow> = self
            .diagnostics
            .iter()
            .zip(messages)
            .enumerate()
            .map(|(at, (diagnostic, message))| {
                let field =
                    rows_at + (at * size_of::<WfRow>()) as u64 + offset_of!(WfRow, message) as u64;
                let (site, def, id, index) = encode_site(diagnostic.site);
                let Some(code) = CODES.iter().position(|&it| it == diagnostic.code) else {
                    panic!("a well-formedness code not in `CODES`: {}", diagnostic.code)
                };
                WfRow {
                    id,
                    message: Slice::between(field, message, diagnostic.message.len()),
                    kind: u32::from(site) | (u32::from(def) << 8) | ((code as u32) << 16),
                    index,
                }
            })
            .collect(); // alloc: the memo's persisted form, once per write
        let written = out.push_slice(&rows);
        debug_assert!(rows.is_empty() || written == rows_at);
        let at = out.next::<WfRoot>();
        let field = at + offset_of!(WfRoot, rows) as u64;
        let root = WfRoot {
            rows: match rows.is_empty() {
                true => Slice::EMPTY,
                false => Slice::between(field, rows_at, rows.len()),
            },
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<WfResultRef<'_>, FormatError> {
        let root = open_as::<WfRoot>(buffer, Self::SCHEMA)?;
        let rows = root.rows.follow(buffer)?;
        Ok(WfResultRef { buffer, rows })
    }
}

/// The site's variant, the kind of its definition, the definition's id and the site's index.
fn encode_site(site: Site) -> (u8, u8, u64, u32) {
    fn bits(id: impl AsId) -> u64 {
        u64::from(id.as_id().as_u32())
    }
    match site {
        Site::Field(variant, field) => {
            let (def, id) = match variant {
                VariantId::EnumVariantId(it) => (0, bits(it)),
                VariantId::StructId(it) => (1, bits(it)),
                VariantId::UnionId(it) => (2, bits(it)),
            };
            (0, def, id, u32::from(field.into_raw()))
        }
        Site::AssocType(it) => (1, 0, bits(it), 0),
        Site::ImplTrait(it) => (2, 0, bits(it), 0),
        Site::Bound(def, index) => {
            let (kind, id) = match def {
                GenericDefId::AdtId(AdtId::StructId(it)) => (0, bits(it)),
                GenericDefId::AdtId(AdtId::UnionId(it)) => (1, bits(it)),
                GenericDefId::AdtId(AdtId::EnumId(it)) => (2, bits(it)),
                GenericDefId::ConstId(it) => (3, bits(it)),
                GenericDefId::FunctionId(it) => (4, bits(it)),
                GenericDefId::ImplId(it) => (5, bits(it)),
                GenericDefId::StaticId(it) => (6, bits(it)),
                GenericDefId::TraitId(it) => (7, bits(it)),
                GenericDefId::TypeAliasId(it) => (8, bits(it)),
            };
            (3, kind, id, index)
        }
        Site::Opaque(it, index) => (4, 0, bits(it), index),
        Site::Param(it, index) => (5, 0, bits(it), index),
        Site::Output(it) => (6, 0, bits(it), 0),
        Site::SelfTy(it) => (7, 0, bits(it), 0),
    }
}

fn decode<'a>(row: &'a WfRow, buffer: &'a [u8]) -> Result<WfDiagnosticRef<'a>, FormatError> {
    let at = (row as *const WfRow as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    let invalid = |what| FormatError::Invalid { at, what };
    if row.id == 0 || row.id > u64::from(u32::MAX) {
        return Err(invalid("an id of 0 or wider than 32 bits"));
    }
    fn id<T: FromId>(bits: u64) -> T {
        T::from_id(blaze_query::db::Id::from_u32(bits as u32))
    }
    let (site, def, code) = (row.kind & 0xff, (row.kind >> 8) & 0xff, row.kind >> 16);
    let bad_def = || invalid("a well-formedness site of an unknown definition kind");
    let site = match site {
        0 => {
            let variant = match def {
                0 => VariantId::EnumVariantId(id::<EnumVariantId>(row.id)),
                1 => VariantId::StructId(id::<StructId>(row.id)),
                2 => VariantId::UnionId(id::<UnionId>(row.id)),
                _ => return Err(bad_def()),
            };
            Site::Field(variant, Idx::from_raw(RawIdx::from(row.index)))
        }
        1 => Site::AssocType(id::<TypeAliasId>(row.id)),
        2 => Site::ImplTrait(id::<ImplId>(row.id)),
        3 => {
            let def = match def {
                0 => GenericDefId::AdtId(AdtId::StructId(id::<StructId>(row.id))),
                1 => GenericDefId::AdtId(AdtId::UnionId(id::<UnionId>(row.id))),
                2 => GenericDefId::AdtId(AdtId::EnumId(id::<EnumId>(row.id))),
                3 => GenericDefId::ConstId(id::<ConstId>(row.id)),
                4 => GenericDefId::FunctionId(id::<FunctionId>(row.id)),
                5 => GenericDefId::ImplId(id::<ImplId>(row.id)),
                6 => GenericDefId::StaticId(id::<StaticId>(row.id)),
                7 => GenericDefId::TraitId(id::<TraitId>(row.id)),
                8 => GenericDefId::TypeAliasId(id::<TypeAliasId>(row.id)),
                _ => return Err(bad_def()),
            };
            Site::Bound(def, row.index)
        }
        4 => Site::Opaque(id::<FunctionId>(row.id), row.index),
        5 => Site::Param(id::<FunctionId>(row.id), row.index),
        6 => Site::Output(id::<FunctionId>(row.id)),
        7 => Site::SelfTy(id::<ImplId>(row.id)),
        _ => return Err(invalid("a well-formedness site of an unknown variant")),
    };
    let code =
        *CODES.get(code as usize).ok_or_else(|| invalid("an unknown well-formedness code"))?;
    let message = row.message.as_str(buffer)?;
    Ok(WfDiagnosticRef { code, message, site })
}

#[cfg(test)]
mod tests {
    use blaze_format::{Aligned, Persist, to_bytes};
    use hir_def::{FunctionId, GenericDefId, ImplId, StructId, VariantId};
    use la_arena::{Idx, RawIdx};
    use blaze_query::db::FromId;

    use crate::{Site, WfDiagnostic, WfResult};

    fn id<T: FromId>(index: u32) -> T {
        T::from_id(blaze_query::db::Id::from_u32(index + (3 << 20)))
    }

    fn round_trip(result: &WfResult) -> WfResult {
        let bytes = Aligned::copy_from(&to_bytes(result));
        WfResult::read(&bytes).unwrap().to_result().unwrap()
    }

    #[test]
    fn a_result_comes_back_equal() {
        let function: FunctionId = id(7);
        let sites = [
            Site::Field(VariantId::StructId(id::<StructId>(2)), Idx::from_raw(RawIdx::from(4))),
            Site::Bound(GenericDefId::FunctionId(function), 3),
            Site::Opaque(function, 1),
            Site::Param(function, 0),
            Site::Output(function),
            Site::SelfTy(id::<ImplId>(9)),
            Site::ImplTrait(id::<ImplId>(9)),
        ];
        let diagnostics = sites
            .into_iter()
            .zip(["E0309", "E0310", "E0311", "E0477"].into_iter().cycle())
            .enumerate()
            .map(|(at, (site, code))| WfDiagnostic { code, message: format!("{code} {at}"), site })
            .collect();
        let result = WfResult { diagnostics };
        assert_eq!(round_trip(&result), result);
    }

    #[test]
    fn a_message_is_read_in_place() {
        let site = Site::Param(id::<FunctionId>(5), 2);
        let result = WfResult {
            diagnostics: vec![WfDiagnostic { code: "E0309", message: "in place".to_owned(), site }],
        };
        let bytes = Aligned::copy_from(&to_bytes(&result));
        let read = WfResult::read(&bytes).unwrap();
        let first = read.iter().next().unwrap().unwrap();
        assert_eq!((first.code, first.message, first.site), ("E0309", "in place", site));
        assert!(bytes.as_ptr_range().contains(&first.message.as_ptr()), "borrowed, not copied");
    }

    #[test]
    fn an_empty_result_comes_back_empty() {
        let bytes = Aligned::copy_from(&to_bytes(&WfResult::default()));
        let read = WfResult::read(&bytes).unwrap();
        assert!(read.is_empty());
        assert_eq!(read.to_result().unwrap(), WfResult::default());
    }

    #[test]
    fn equal_results_are_equal_bytes() {
        let site = Site::Output(id::<FunctionId>(5));
        let result = WfResult {
            diagnostics: vec![WfDiagnostic { code: "E0310", message: "m".to_owned(), site }],
        };
        assert_eq!(to_bytes(&result), to_bytes(&result.clone()));
    }

    #[test]
    fn a_result_of_another_schema_is_refused() {
        let bytes = Aligned::copy_from(&to_bytes(&blaze_format::Spans::new(Default::default())));
        assert!(matches!(WfResult::read(&bytes), Err(blaze_format::FormatError::Schema { .. })));
    }
}
