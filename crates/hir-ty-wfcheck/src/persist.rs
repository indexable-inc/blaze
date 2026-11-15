//! [`WfResult`] in blaze's format, readable by another process: one row per diagnostic, its site
//! as a tag word and the definition it is in as an index in the record's own table of its
//! crate's items (`hir_def::deps::PortableIds`, the signatures part's portable encoding), its
//! message in the buffer. A reader makes the table's items again in its own database, so no
//! word of the record is an id of the session that wrote it.

use std::alloc::{Allocator, Global};
use std::fmt;
use std::mem::offset_of;

use base_db::{Crate, Db};
use blaze_format::{FormatError, Record, SchemaHash, Slice, Str, Writer, open_as};
use hir_def::{
    AdtId, ConstId, EnumId, EnumVariantId, FunctionId, GenericDefId, ImplId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId, VariantId,
    deps::{PortableId, PortableIds},
};
use la_arena::{Idx, RawIdx};

use crate::{Site, WfDiagnostic, WfResult};

blaze_format::record! {
    /// One diagnostic. `id` is the index, in the record's table, of the definition the site is
    /// in; `kind` holds the site's variant in its low byte, the kind of that definition in the
    /// next, and the error code in the third.
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
    struct WfRoot: 2 {
        rows: Slice<WfRow>,
        /// The crate's portable table of the definitions the rows name ([`PortableIds::encode`]).
        items: Str,
    }
}

const _: () = assert!(size_of::<WfRow>() == 24);

/// The schema of a [`WfResult`] buffer: version 2, whose rows name definitions by their index in
/// the record's portable table.
pub const SCHEMA: SchemaHash = blaze_format::schema_of(&[
    blaze_format::Part::Str("WfResult"),
    blaze_format::Part::U64(2),
    blaze_format::Part::Schema(WfRoot::SCHEMA),
]);

/// The schema of the previous layout, version 1: a root of rows alone, each `id` the bits of an
/// id of the session that wrote it, which means nothing in another process. Refused by name.
const PREVIOUS_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    blaze_format::Part::Str("WfResult"),
    blaze_format::Part::U64(1),
    blaze_format::Part::Schema(blaze_format::schema_of(&[
        blaze_format::Part::Str("WfRoot"),
        blaze_format::Part::U64(1),
        blaze_format::Part::Str("rows"),
        blaze_format::Part::U64(0),
        blaze_format::Part::Schema(<Slice<WfRow> as Record>::SCHEMA),
    ])),
]);

/// The codes a check reports, by their number in a row.
const CODES: [&str; 4] = ["E0309", "E0310", "E0311", "E0477"];

/// Why bytes are not a [`WfResult`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WfReadError {
    /// A buffer of the previous layout (version 1), whose ids were one session's.
    PreviousLayout,
    /// The bytes are not a valid buffer of this layout.
    Format(FormatError),
    /// The record's table of items does not decode in this database: the table's own reason.
    Items(String),
    /// The row at byte `at` names entry `index` of the record's table as a `kind`, and the table
    /// has no such item there.
    NoItem { at: u64, index: u64, kind: &'static str },
}

impl fmt::Display for WfReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WfReadError::PreviousLayout => write!(
                f,
                "a well-formedness result of layout 1 (session ids), and this blaze reads layout 2 (portable ids)"
            ),
            WfReadError::Format(error) => write!(f, "a well-formedness result: {error}"),
            WfReadError::Items(why) => {
                write!(f, "a well-formedness result's table of items does not decode: {why}")
            }
            WfReadError::NoItem { at, index, kind } => write!(
                f,
                "the well-formedness row at byte {at} names item {index} of its table as a `{kind}`, and the table has none"
            ),
        }
    }
}

impl std::error::Error for WfReadError {}

/// Why a [`WfResult`] is not written ([`WfResult::to_bytes`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WfWriteError {
    /// A definition a site names is not one the crate's portable table of items can write:
    /// the table's own reason (`hir_def::deps::PortableIds::encode`).
    Items(String),
}

impl fmt::Display for WfWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WfWriteError::Items(why) => {
                write!(f, "a well-formedness result's table of items is not written: {why}")
            }
        }
    }
}

impl std::error::Error for WfWriteError {}

impl From<FormatError> for WfReadError {
    fn from(error: FormatError) -> WfReadError {
        WfReadError::Format(error)
    }
}

/// A [`WfResult`] read in place.
#[derive(Clone, Copy, Debug)]
pub struct WfResultRef<'a> {
    buffer: &'a [u8],
    rows: &'a [WfRow],
    items: &'a [u8],
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

    /// The record's table of items, made again in `db`, of `krate` (the crate the record's
    /// result is of).
    pub fn ids(&self, db: &Db, krate: Crate) -> Result<PortableIds, WfReadError> {
        PortableIds::decode(db, krate, self.items).map_err(WfReadError::Items)
    }

    /// The diagnostics in place, in order, their definitions looked up in `ids` ([`Self::ids`]).
    pub fn iter<'i>(
        &self,
        ids: &'i PortableIds,
    ) -> impl Iterator<Item = Result<WfDiagnosticRef<'a>, WfReadError>> + 'i
    where
        'a: 'i,
    {
        let buffer = self.buffer;
        self.rows.iter().map(move |row| decode(row, buffer, ids))
    }

    /// The result as the query returns it, in `db`.
    pub fn to_result(&self, db: &Db, krate: Crate) -> Result<WfResult, WfReadError> {
        let ids = self.ids(db, krate)?;
        let mut diagnostics = Vec::with_capacity_in(self.rows.len(), Global);
        for it in self.iter(&ids) {
            let WfDiagnosticRef { code, message, site } = it?;
            diagnostics.push(WfDiagnostic { code, message: message.to_owned(), site });
        }
        Ok(WfResult { diagnostics })
    }
}

impl WfResult {
    /// The result in blaze's format: every definition a site names is written as its index in a
    /// portable table of `krate`'s items, the table in the buffer. An error names a definition
    /// the table cannot write ([`PortableIds::encode`]).
    pub fn to_bytes<A: Allocator>(
        &self,
        db: &Db,
        krate: Crate,
        alloc: A,
    ) -> Result<Vec<u8, A>, WfWriteError> {
        let mut ids = PortableIds::new();
        let mut sites = Vec::with_capacity_in(self.diagnostics.len(), Global);
        for diagnostic in &self.diagnostics {
            sites.push(encode_site(&mut ids, diagnostic.site));
        }
        let table = ids.encode(db, krate).map_err(WfWriteError::Items)?;
        let mut out = Writer::new_in(SCHEMA, alloc);
        let text: usize = self.diagnostics.iter().map(|it| it.message.len()).sum();
        out.bytes.reserve(
            text + table.len() + (self.diagnostics.len() + 1) * size_of::<WfRow>() + size_of::<WfRoot>(),
        );
        let mut messages = Vec::with_capacity_in(self.diagnostics.len(), Global);
        for diagnostic in &self.diagnostics {
            messages.push(out.push_str(&diagnostic.message));
        }
        let items_at = out.push_slice(&table);
        let rows_at = out.next::<WfRow>();
        let mut rows = Vec::with_capacity_in(self.diagnostics.len(), Global);
        for (at, ((diagnostic, message), &(site, def, id, index))) in
            self.diagnostics.iter().zip(messages).zip(sites.iter()).enumerate()
        {
            let field =
                rows_at + (at * size_of::<WfRow>()) as u64 + offset_of!(WfRow, message) as u64;
            let Some(code) = CODES.iter().position(|&it| it == diagnostic.code) else {
                panic!("a well-formedness code not in `CODES`: {}", diagnostic.code)
            };
            rows.push(WfRow {
                id,
                message: Slice::between(field, message, diagnostic.message.len()),
                kind: u32::from(site) | (u32::from(def) << 8) | ((code as u32) << 16),
                index,
            });
        }
        let written = out.push_slice(&rows);
        debug_assert!(rows.is_empty() || written == rows_at);
        let at = out.next::<WfRoot>();
        let root = WfRoot {
            rows: match rows.is_empty() {
                true => Slice::EMPTY,
                false => Slice::between(at + offset_of!(WfRoot, rows) as u64, rows_at, rows.len()),
            },
            items: match table.is_empty() {
                true => Slice::EMPTY,
                false => {
                    Slice::between(at + offset_of!(WfRoot, items) as u64, items_at, table.len())
                }
            },
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        Ok(out.finish(at))
    }

    /// A buffer [`Self::to_bytes`] wrote, read in place; a buffer of the previous layout is
    /// [`WfReadError::PreviousLayout`].
    pub fn read(buffer: &[u8]) -> Result<WfResultRef<'_>, WfReadError> {
        let root = open_as::<WfRoot>(buffer, SCHEMA).map_err(|error| match error {
            FormatError::Schema { found, .. } if found == PREVIOUS_SCHEMA => {
                WfReadError::PreviousLayout
            }
            other => WfReadError::Format(other),
        })?;
        let rows = root.rows.follow(buffer)?;
        let items = root.items.follow(buffer)?;
        Ok(WfResultRef { buffer, rows, items })
    }
}

/// The site's variant, the kind of its definition, the definition's index in `ids` and the
/// site's index.
fn encode_site(ids: &mut PortableIds, site: Site) -> (u8, u8, u64, u32) {
    match site {
        Site::Field(variant, field) => {
            let (def, id) = match variant {
                VariantId::EnumVariantId(it) => (0, ids.index(it)),
                VariantId::StructId(it) => (1, ids.index(it)),
                VariantId::UnionId(it) => (2, ids.index(it)),
            };
            (0, def, u64::from(id), u32::from(field.into_raw()))
        }
        Site::AssocType(it) => (1, 0, u64::from(ids.index(it)), 0),
        Site::ImplTrait(it) => (2, 0, u64::from(ids.index(it)), 0),
        Site::Bound(def, index) => {
            let (kind, id) = match def {
                GenericDefId::AdtId(AdtId::StructId(it)) => (0, ids.index(it)),
                GenericDefId::AdtId(AdtId::UnionId(it)) => (1, ids.index(it)),
                GenericDefId::AdtId(AdtId::EnumId(it)) => (2, ids.index(it)),
                GenericDefId::ConstId(it) => (3, ids.index(it)),
                GenericDefId::FunctionId(it) => (4, ids.index(it)),
                GenericDefId::ImplId(it) => (5, ids.index(it)),
                GenericDefId::StaticId(it) => (6, ids.index(it)),
                GenericDefId::TraitId(it) => (7, ids.index(it)),
                GenericDefId::TypeAliasId(it) => (8, ids.index(it)),
            };
            (3, kind, u64::from(id), index)
        }
        Site::Opaque(it, index) => (4, 0, u64::from(ids.index(it)), index),
        Site::Param(it, index) => (5, 0, u64::from(ids.index(it)), index),
        Site::Output(it) => (6, 0, u64::from(ids.index(it)), 0),
        Site::SelfTy(it) => (7, 0, u64::from(ids.index(it)), 0),
    }
}

fn decode<'a>(
    row: &'a WfRow,
    buffer: &'a [u8],
    ids: &PortableIds,
) -> Result<WfDiagnosticRef<'a>, WfReadError> {
    let at = (row as *const WfRow as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    let invalid = |what| WfReadError::Format(FormatError::Invalid { at, what });
    let word = row.id;
    fn id<T: PortableId>(ids: &PortableIds, at: u64, word: u64) -> Result<T, WfReadError> {
        u32::try_from(word).ok().and_then(|index| ids.get::<T>(index)).ok_or(WfReadError::NoItem {
            at,
            index: word,
            kind: std::any::type_name::<T>(),
        })
    }
    let (site, def, code) = (row.kind & 0xff, (row.kind >> 8) & 0xff, row.kind >> 16);
    let bad_def = || invalid("a well-formedness site of an unknown definition kind");
    let site = match site {
        0 => {
            let variant = match def {
                0 => VariantId::EnumVariantId(id::<EnumVariantId>(ids, at, word)?),
                1 => VariantId::StructId(id::<StructId>(ids, at, word)?),
                2 => VariantId::UnionId(id::<UnionId>(ids, at, word)?),
                _ => return Err(bad_def()),
            };
            Site::Field(variant, Idx::from_raw(RawIdx::from(row.index)))
        }
        1 => Site::AssocType(id::<TypeAliasId>(ids, at, word)?),
        2 => Site::ImplTrait(id::<ImplId>(ids, at, word)?),
        3 => {
            let def = match def {
                0 => GenericDefId::AdtId(AdtId::StructId(id::<StructId>(ids, at, word)?)),
                1 => GenericDefId::AdtId(AdtId::UnionId(id::<UnionId>(ids, at, word)?)),
                2 => GenericDefId::AdtId(AdtId::EnumId(id::<EnumId>(ids, at, word)?)),
                3 => GenericDefId::ConstId(id::<ConstId>(ids, at, word)?),
                4 => GenericDefId::FunctionId(id::<FunctionId>(ids, at, word)?),
                5 => GenericDefId::ImplId(id::<ImplId>(ids, at, word)?),
                6 => GenericDefId::StaticId(id::<StaticId>(ids, at, word)?),
                7 => GenericDefId::TraitId(id::<TraitId>(ids, at, word)?),
                8 => GenericDefId::TypeAliasId(id::<TypeAliasId>(ids, at, word)?),
                _ => return Err(bad_def()),
            };
            Site::Bound(def, row.index)
        }
        4 => Site::Opaque(id::<FunctionId>(ids, at, word)?, row.index),
        5 => Site::Param(id::<FunctionId>(ids, at, word)?, row.index),
        6 => Site::Output(id::<FunctionId>(ids, at, word)?),
        7 => Site::SelfTy(id::<ImplId>(ids, at, word)?),
        _ => return Err(invalid("a well-formedness site of an unknown variant")),
    };
    let code =
        *CODES.get(code as usize).ok_or_else(|| invalid("an unknown well-formedness code"))?;
    let message = row.message.as_str(buffer)?;
    Ok(WfDiagnosticRef { code, message, site })
}

#[cfg(test)]
mod tests {
    use base_db::{Crate, Db};
    use blaze_format::{Aligned, Slice, Writer};
    use hir_def::{
        FunctionId, GenericDefId, ImplId, ModuleDefId, StructId, VariantId,
        nameres::crate_def_map,
    };
    use hir_expand::fixture;
    use la_arena::{Idx, RawIdx};
    use std::alloc::Global;

    use super::{PREVIOUS_SCHEMA, WfReadError, WfRow};
    use crate::{Site, WfDiagnostic, WfResult, outlives::tests::db::TestDB};

    const FIXTURE: &str = r#"
struct S<'a, T> { x: &'a T, y: u8 }
fn f<'a, T>(x: &'a T) -> &'a T { x }
trait Tr {}
impl<'a, T> Tr for S<'a, T> {}
"#;

    /// A fresh database of [`FIXTURE`] and its crate: each is another process's view of it.
    fn fixture_db() -> (TestDB, Crate) {
        let fx = fixture::db(FIXTURE);
        let _ = fx.only_file();
        let krate = fx.krate();
        (fx.into_db(), krate)
    }

    /// The fixture's struct, function and impl in `db`.
    fn items(db: &Db, krate: Crate) -> (StructId, FunctionId, ImplId) {
        let (mut strukt, mut function, mut impl_) = (None, None, None);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                match def {
                    ModuleDefId::AdtId(hir_def::AdtId::StructId(it)) => strukt = strukt.or(Some(it)),
                    ModuleDefId::FunctionId(it) => function = function.or(Some(it)),
                    _ => (),
                }
            }
            impl_ = impl_.or(module.scope(db).impls().next());
        }
        (strukt.expect("the struct"), function.expect("the function"), impl_.expect("the impl"))
    }

    /// One diagnostic at each kind of site, of the fixture's items in `db`.
    fn result(db: &Db, krate: Crate) -> WfResult {
        let (strukt, function, impl_) = items(db, krate);
        let sites = [
            Site::Field(VariantId::StructId(strukt), Idx::from_raw(RawIdx::from(1))),
            Site::Bound(GenericDefId::FunctionId(function), 3),
            Site::Bound(GenericDefId::AdtId(hir_def::AdtId::StructId(strukt)), 0),
            Site::Opaque(function, 1),
            Site::Param(function, 0),
            Site::Output(function),
            Site::SelfTy(impl_),
            Site::ImplTrait(impl_),
        ];
        let diagnostics = sites
            .into_iter()
            .zip(["E0309", "E0310", "E0311", "E0477"].into_iter().cycle())
            .enumerate()
            .map(|(at, (site, code))| WfDiagnostic { code, message: format!("{code} {at}"), site })
            .collect();
        WfResult { diagnostics }
    }

    /// rule 26: written by one database and read by another, a result names the same items,
    /// made again in the reader.
    #[test]
    fn a_result_written_by_one_database_comes_back_equal_in_another() {
        let (writer, writer_crate) = fixture_db();
        let bytes = blaze_memo::attach(&writer, || {
            result(&writer, writer_crate).to_bytes(&writer, writer_crate, Global).unwrap()
        });
        let (reader, reader_crate) = fixture_db();
        blaze_memo::attach(&reader, || {
            let bytes = Aligned::copy_from(&bytes);
            let read = WfResult::read(&bytes).unwrap().to_result(&reader, reader_crate).unwrap();
            assert_eq!(read, result(&reader, reader_crate));
        });
    }

    #[test]
    fn a_message_is_read_in_place() {
        let (db, krate) = fixture_db();
        blaze_memo::attach(&db, || {
            let (_, function, _) = items(&db, krate);
            let site = Site::Param(function, 2);
            let result = WfResult {
                diagnostics: vec![WfDiagnostic {
                    code: "E0309",
                    message: "in place".to_owned(),
                    site,
                }],
            };
            let bytes = Aligned::copy_from(&result.to_bytes(&db, krate, Global).unwrap());
            let read = WfResult::read(&bytes).unwrap();
            let ids = read.ids(&db, krate).unwrap();
            let first = read.iter(&ids).next().unwrap().unwrap();
            assert_eq!((first.code, first.message, first.site), ("E0309", "in place", site));
            assert!(bytes.as_ptr_range().contains(&first.message.as_ptr()), "borrowed, not copied");
        });
    }

    #[test]
    fn an_empty_result_comes_back_empty() {
        let (db, krate) = fixture_db();
        blaze_memo::attach(&db, || {
            let bytes = Aligned::copy_from(&WfResult::default().to_bytes(&db, krate, Global).unwrap());
            let read = WfResult::read(&bytes).unwrap();
            assert!(read.is_empty());
            assert_eq!(read.to_result(&db, krate).unwrap(), WfResult::default());
        });
    }

    #[test]
    fn equal_results_are_equal_bytes() {
        let (db, krate) = fixture_db();
        blaze_memo::attach(&db, || {
            let result = result(&db, krate);
            assert_eq!(
                result.to_bytes(&db, krate, Global).unwrap(),
                result.clone().to_bytes(&db, krate, Global).unwrap()
            );
        });
    }

    /// rule 26: a restored row whose id names no item of its kind in the record's table (an
    /// index past the table, past 32 bits, or an item of another kind) is the row's typed error
    /// naming the byte, the index and the kind, never a panic or another item.
    #[test]
    fn a_persisted_id_that_names_no_item_is_a_typed_error_naming_it() {
        let (db, krate) = fixture_db();
        blaze_memo::attach(&db, || {
            let (_, function, impl_) = items(&db, krate);
            let result = WfResult {
                diagnostics: vec![
                    WfDiagnostic { code: "E0310", message: "m".to_owned(), site: Site::Output(function) },
                    WfDiagnostic { code: "E0310", message: "n".to_owned(), site: Site::SelfTy(impl_) },
                ],
            };
            let written = result.to_bytes(&db, krate, Global).unwrap();
            let rows_at = {
                let bytes = Aligned::copy_from(&written);
                let read = WfResult::read(&bytes).unwrap();
                (read.rows.as_ptr() as usize) - (bytes.as_ptr() as usize)
            };
            let row = rows_at;
            let word = row + std::mem::offset_of!(WfRow, id);
            let function_kind = std::any::type_name::<FunctionId>();
            for bad in [2, 1000, 1 << 32, u64::MAX] {
                let mut bytes = written.clone();
                bytes[word..word + 8].copy_from_slice(&bad.to_ne_bytes());
                let bytes = Aligned::copy_from(&bytes);
                let read = WfResult::read(&bytes).expect("only the row is corrupt");
                let error = read.to_result(&db, krate).expect_err("an id that names no item");
                assert_eq!(
                    error,
                    WfReadError::NoItem { at: row as u64, index: bad, kind: function_kind },
                    "the word {bad}"
                );
                assert!(error.to_string().contains(&bad.to_string()), "names the index: {error}");
            }
            // Entry 1 is the impl: the first row reads a function there.
            let mut bytes = written.clone();
            bytes[word..word + 8].copy_from_slice(&1u64.to_ne_bytes());
            let bytes = Aligned::copy_from(&bytes);
            let error = WfResult::read(&bytes).unwrap().to_result(&db, krate).unwrap_err();
            assert_eq!(error, WfReadError::NoItem { at: row as u64, index: 1, kind: function_kind });
            assert!(error.to_string().contains("FunctionId"), "names the kind: {error}");
        });
    }

    /// rule 26: a buffer of the previous layout, whose ids were the writing session's, is refused
    /// by name, never read as this layout's.
    #[test]
    fn a_result_of_the_previous_layout_is_refused_by_name() {
        let mut out = Writer::new(PREVIOUS_SCHEMA);
        let root = out.push(&Slice::<WfRow>::EMPTY);
        let bytes = Aligned::copy_from(&out.finish(root));
        let error = WfResult::read(&bytes).unwrap_err();
        assert_eq!(error, WfReadError::PreviousLayout);
        assert!(error.to_string().contains("layout 1"), "{error}");
    }

    #[test]
    fn a_result_of_another_schema_is_refused() {
        let bytes = Aligned::copy_from(&blaze_format::to_bytes(&blaze_format::Spans::new(
            Default::default(),
        )));
        assert!(matches!(
            WfResult::read(&bytes),
            Err(WfReadError::Format(blaze_format::FormatError::Schema { .. }))
        ));
    }
}
