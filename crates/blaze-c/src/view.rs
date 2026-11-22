//! The declaration record read in place: [`DeclsView::open`] checks every reference once (rule
//! 74), so the rendering and the differential compare index the columns with no check of
//! their own.

use blaze_format_core::{FormatError, open_as};

use crate::canon::{DeclsRoot, decls_schema};
use crate::model::{
    AbiRow, BaseRow, ConstRow, Conv, DeclKind, EnumRow, FieldRow, FloatKind, FnRow, HeaderRow, IntKind, Linkage,
    MacroRow, MethodKind, NamespaceRow, RecordKind, RecordRow, Relocation, TypeRow, TypedefRow, Unexpressed,
    UnexpressedRow, UnitRow, ValueCategory, VarRow, init, is_c_identifier, tag, value,
};
use crate::{NONE, at, narrow};

/// A column of the record: where a check or a difference is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Table {
    Names,
    Headers,
    Namespaces,
    Types,
    TypeLists,
    Records,
    Fields,
    Units,
    Bases,
    Enums,
    Consts,
    Fns,
    NameLists,
    Vars,
    Typedefs,
    Macros,
    Strings,
    Unexpressed,
    Abi,
}

/// Why bytes are not a declaration record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ViewError {
    Format(FormatError),
    /// Row `row` of `table` is not a row of a record [`crate::canon::encode`] writes.
    Invalid { table: Table, row: u32, what: &'static str },
}

/// A checked declaration record.
#[derive(Clone, Copy)]
pub struct DeclsView<'b> {
    pub name_bytes: &'b [u8],
    pub name_ends: &'b [u32],
    pub headers: &'b [HeaderRow],
    pub namespaces: &'b [NamespaceRow],
    pub types: &'b [TypeRow],
    pub type_lists: &'b [u32],
    pub records: &'b [RecordRow],
    pub fields: &'b [FieldRow],
    pub units: &'b [UnitRow],
    pub bases: &'b [BaseRow],
    pub enums: &'b [EnumRow],
    pub consts: &'b [ConstRow],
    pub fns: &'b [FnRow],
    pub name_lists: &'b [u32],
    pub vars: &'b [VarRow],
    pub typedefs: &'b [TypedefRow],
    pub macros: &'b [MacroRow],
    pub strings: &'b [u8],
    pub unexpressed: &'b [UnexpressedRow],
    pub abi: AbiRow,
}

fn range(start: u32, len: u32, of: usize) -> Option<core::ops::Range<usize>> {
    let end = at(start).checked_add(at(len))?;
    (end <= of).then_some(at(start)..end)
}

impl<'b> DeclsView<'b> {
    pub fn open(buffer: &'b [u8]) -> Result<DeclsView<'b>, ViewError> {
        let root: &DeclsRoot = open_as::<DeclsRoot>(buffer, decls_schema()).map_err(ViewError::Format)?;
        let f = ViewError::Format;
        let view = DeclsView {
            name_bytes: root.name_bytes.follow(buffer).map_err(f)?,
            name_ends: root.name_ends.follow(buffer).map_err(f)?,
            headers: root.headers.follow(buffer).map_err(f)?,
            namespaces: root.namespaces.follow(buffer).map_err(f)?,
            types: root.types.follow(buffer).map_err(f)?,
            type_lists: root.type_lists.follow(buffer).map_err(f)?,
            records: root.records.follow(buffer).map_err(f)?,
            fields: root.fields.follow(buffer).map_err(f)?,
            units: root.units.follow(buffer).map_err(f)?,
            bases: root.bases.follow(buffer).map_err(f)?,
            enums: root.enums.follow(buffer).map_err(f)?,
            consts: root.consts.follow(buffer).map_err(f)?,
            fns: root.fns.follow(buffer).map_err(f)?,
            name_lists: root.name_lists.follow(buffer).map_err(f)?,
            vars: root.vars.follow(buffer).map_err(f)?,
            typedefs: root.typedefs.follow(buffer).map_err(f)?,
            macros: root.macros.follow(buffer).map_err(f)?,
            strings: root.strings.follow(buffer).map_err(f)?,
            unexpressed: root.unexpressed.follow(buffer).map_err(f)?,
            abi: root.abi,
        };
        view.check()?;
        Ok(view)
    }

    /// The text of name `name` (a checked index, or a panic: every index in a checked view is).
    pub fn name(&self, name: u32) -> &'b str {
        let k = at(name);
        let start = if k == 0 { 0 } else { at(self.name_ends[k - 1]) };
        core::str::from_utf8(&self.name_bytes[start..at(self.name_ends[k])]).expect("checked by open")
    }

    /// `name`'s text, `None` for [`NONE`].
    pub fn opt_name(&self, name: u32) -> Option<&'b str> {
        (name != NONE).then(|| self.name(name))
    }

    pub fn fields_of(&self, record: &RecordRow) -> &'b [FieldRow] {
        &self.fields[at(record.fields_start)..at(record.fields_start) + at(record.fields_len)]
    }

    pub fn units_of(&self, record: &RecordRow) -> &'b [UnitRow] {
        &self.units[at(record.units_start)..at(record.units_start) + at(record.units_len)]
    }

    pub fn bases_of(&self, record: &RecordRow) -> &'b [BaseRow] {
        &self.bases[at(record.bases_start)..at(record.bases_start) + at(record.bases_len)]
    }

    pub fn consts_of(&self, decl: &EnumRow) -> &'b [ConstRow] {
        &self.consts[at(decl.consts_start)..at(decl.consts_start) + at(decl.consts_len)]
    }

    /// A function type's parameter types.
    pub fn params_of(&self, ty: &TypeRow) -> &'b [u32] {
        &self.type_lists[at(ty.b)..at(ty.b) + at(ty.c)]
    }

    pub fn param_names(&self, f: &FnRow) -> &'b [u32] {
        &self.name_lists[at(f.params_start)..at(f.params_start) + at(f.params_len)]
    }

    pub fn string_of(&self, m: &MacroRow) -> &'b [u8] {
        &self.strings[at(m.str_start)..at(m.str_start) + at(m.str_len)]
    }

    fn check(&self) -> Result<(), ViewError> {
        let bad = |table, row: usize, what| ViewError::Invalid { table, row: narrow(row).unwrap_or(NONE), what };
        // Names: ends rising, inside the bytes, each a C identifier, sorted and each once.
        let mut start = 0usize;
        let mut last: Option<&[u8]> = None;
        for (k, end) in self.name_ends.iter().enumerate() {
            let end = at(*end);
            if end < start || end > self.name_bytes.len() {
                return Err(bad(Table::Names, k, "an end outside the bytes or before the start"));
            }
            let bytes = &self.name_bytes[start..end];
            let text = core::str::from_utf8(bytes).map_err(|_| bad(Table::Names, k, "not UTF-8"))?;
            if !is_c_identifier(text) {
                return Err(bad(Table::Names, k, "not a C identifier"));
            }
            if last.is_some_and(|l| l >= bytes) {
                return Err(bad(Table::Names, k, "names out of order or repeated"));
            }
            last = Some(bytes);
            start = end;
        }
        if start != self.name_bytes.len() {
            return Err(bad(Table::Names, self.name_ends.len(), "bytes past the last name"));
        }
        let names = self.name_ends.len();
        let name = |n: u32| at(n) < names;
        let opt_name = |n: u32| n == NONE || name(n);
        let header = |h: u32| at(h) < self.headers.len();
        let ns = |n: u32| n == NONE || at(n) < self.namespaces.len();
        for (k, row) in self.headers.iter().enumerate() {
            if crate::place::Root::from_byte(row.root).is_none() || row.pad != [0; 3] {
                return Err(bad(Table::Headers, k, "a root byte no root has, or padding not zero"));
            }
            if k > 0 && self.headers[k - 1] >= *row {
                return Err(bad(Table::Headers, k, "headers out of order or repeated"));
            }
        }
        for (k, row) in self.namespaces.iter().enumerate() {
            if !name(row.name) || !(row.parent == NONE || at(row.parent) < k) {
                return Err(bad(Table::Namespaces, k, "a name or parent out of range"));
            }
        }
        let types = self.types.len();
        for (k, row) in self.types.iter().enumerate() {
            let earlier = |t: u32| at(t) < k;
            let ok = match row.kind {
                tag::VOID => true,
                tag::BOOL => true,
                tag::INT => IntKind::from_byte(row.extra).is_some(),
                tag::FLOAT => FloatKind::from_byte(row.extra).is_some(),
                tag::POINTER | tag::VA_LIST => earlier(row.a),
                tag::REFERENCE => earlier(row.a) && ValueCategory::from_byte(row.extra).is_some(),
                tag::ARRAY => earlier(row.a),
                tag::FUNCTION => {
                    earlier(row.a)
                        && Conv::from_byte(row.extra).is_some()
                        && range(row.b, row.c, self.type_lists.len()).is_some_and(|r| self.type_lists[r].iter().all(|p| earlier(*p)))
                }
                tag::RECORD => at(row.a) < self.records.len(),
                tag::ENUM => at(row.a) < self.enums.len(),
                tag::TYPEDEF => at(row.a) < self.typedefs.len(),
                tag::UNEXPRESSIBLE => Unexpressed::from_byte(row.extra).is_some(),
                _ => false,
            };
            if !ok || row.kind > tag::LAST {
                return Err(bad(Table::Types, k, "a kind, a reference or a list no type has"));
            }
        }
        let ty = |t: u32| at(t) < types;
        for (k, row) in self.records.iter().enumerate() {
            let fields = range(row.fields_start, row.fields_len, self.fields.len());
            let units = range(row.units_start, row.units_len, self.units.len());
            let bases = range(row.bases_start, row.bases_len, self.bases.len());
            let (Some(fields), Some(_), Some(bases)) = (fields, units, bases) else {
                return Err(bad(Table::Records, k, "fields, units or bases out of range"));
            };
            if !opt_name(row.name)
                || !ns(row.ns)
                || !header(row.header)
                || RecordKind::from_byte(row.kind).is_none()
                || Relocation::from_byte(row.relocation).is_none()
                || row.complete > 1
            {
                return Err(bad(Table::Records, k, "a name, namespace, header or byte out of range"));
            }
            for (j, field) in self.fields[fields.clone()].iter().enumerate() {
                let unit_ok = match field.bitfield {
                    0 => field.unit == NONE && field.width == 0,
                    1 => field.unit >= row.units_start && field.unit - row.units_start < row.units_len,
                    _ => false,
                };
                if !opt_name(field.name) || !ty(field.ty) || !unit_ok {
                    return Err(bad(Table::Fields, fields.start + j, "a name, type or unit out of range"));
                }
            }
            for (j, base) in self.bases[bases.clone()].iter().enumerate() {
                if at(base.record) >= self.records.len() || base.flags > 1 {
                    return Err(bad(Table::Bases, bases.start + j, "a base out of range"));
                }
            }
        }
        for (k, row) in self.enums.iter().enumerate() {
            let Some(consts) = range(row.consts_start, row.consts_len, self.consts.len()) else {
                return Err(bad(Table::Enums, k, "constants out of range"));
            };
            if !opt_name(row.name) || !ns(row.ns) || !header(row.header) || !(row.underlying == NONE || ty(row.underlying)) {
                return Err(bad(Table::Enums, k, "a name, namespace, header or type out of range"));
            }
            if self.consts[consts].iter().any(|c| !name(c.name)) {
                return Err(bad(Table::Enums, k, "a constant's name out of range"));
            }
        }
        for (k, row) in self.fns.iter().enumerate() {
            let params = range(row.params_start, row.params_len, self.name_lists.len());
            let ok = params.is_some_and(|r| self.name_lists[r].iter().all(|n| opt_name(*n)))
                && name(row.name)
                && name(row.symbol)
                && ns(row.ns)
                && ty(row.ty)
                && header(row.header)
                && (row.owner == NONE || at(row.owner) < self.records.len())
                && MethodKind::from_byte(row.method).is_some()
                && Linkage::from_byte(row.linkage).is_some()
                && row.body <= 1
                && row.pad == 0;
            if !ok {
                return Err(bad(Table::Fns, k, "a name, type, owner, list, linkage or body out of range"));
            }
        }
        for (k, row) in self.vars.iter().enumerate() {
            let linkage_ok = match Linkage::from_byte(row.linkage) {
                Some(Linkage::External) => row.init <= init::OTHER,
                // A header-local variable is a definition (C's tentative one is zero).
                Some(Linkage::Internal) => (init::ZERO..=init::OTHER).contains(&row.init),
                Some(Linkage::Inline) | None => false,
            };
            let bits_ok = row.init == init::INT || row.init_bits == 0;
            if !name(row.name)
                || !name(row.symbol)
                || !ns(row.ns)
                || !ty(row.ty)
                || !header(row.header)
                || !linkage_ok
                || !bits_ok
                || row.pad != [0; 6]
            {
                return Err(bad(Table::Vars, k, "a name, type, header, linkage or initializer out of range"));
            }
        }
        for (k, row) in self.typedefs.iter().enumerate() {
            if !name(row.name) || !ns(row.ns) || !ty(row.ty) || !header(row.header) {
                return Err(bad(Table::Typedefs, k, "a name, type or header out of range"));
            }
        }
        for (k, row) in self.macros.iter().enumerate() {
            let var = |v: u32| at(v) < self.vars.len();
            // A constant names no variable; a slot or an address names one, and an address's
            // type is its variable's.
            let refs_ok = match row.kind {
                value::INT | value::FLOAT | value::STR => row.var == NONE && row.deref == 0,
                value::SLOT => var(row.var) && row.deref <= 1 && row.str_len == 0,
                value::ADDRESS_OF => var(row.var) && row.ty == self.vars[at(row.var)].ty && row.deref == 0 && row.value == 0 && row.str_len == 0,
                _ => false,
            };
            let ok = name(row.name)
                && ty(row.ty)
                && header(row.header)
                && row.kind <= value::LAST
                && refs_ok
                && row.pad == [0; 6]
                && range(row.str_start, row.str_len, self.strings.len()).is_some();
            if !ok {
                return Err(bad(Table::Macros, k, "a name, type, kind or string out of range"));
            }
        }
        for (k, row) in self.unexpressed.iter().enumerate() {
            if !name(row.name)
                || !ns(row.ns)
                || !header(row.header)
                || DeclKind::from_byte(row.kind).is_none()
                || Unexpressed::from_byte(row.cause).is_none()
            {
                return Err(bad(Table::Unexpressed, k, "a name, header, kind or cause out of range"));
            }
        }
        Ok(())
    }
}
