//! The declaration record: a [`DeclsBuilder`] written in its one canonical form, so two front
//! ends that read one header the same way write equal bytes (rule 25), whatever order they made
//! rows in and however much they shared.
//!
//! What is sorted, and what keeps its order:
//! - SORTED (order carries no meaning): names (by bytes, each once); headers (by path digest);
//!   namespaces (by depth, parent and name); records and enums with a name (by namespace and
//!   name); typedefs, variables and constant macros (by namespace and name); functions (by
//!   namespace, name and linker symbol: an overload set's members are next to each other; a
//!   function's repeated declarations and its definition are one row, the definition's);
//!   unexpressed rows (by namespace, name, kind, cause and place); anonymous enums (by their
//!   first constant's name, which C's one ordinary namespace makes unique).
//! - KEPT (order is meaning): a record's fields, units and bases; an enum's constants; a
//!   function type's parameters and a function's parameter names; a string's bytes.
//! - NUMBERED BY FIRST REACH: types and anonymous records, in one walk over the sorted
//!   declarations (records' fields, then anonymous records as reached, enums, typedefs,
//!   functions, variables, macros). A type is written after every type it names, and once:
//!   structurally equal types are one row, so a front end that makes `int` once and one that
//!   makes it at each use write the same bytes. An anonymous record nothing reaches declares
//!   nothing (C's "declaration does not declare anything") and is not written.
//!
//! The record's schema is derived from its rows' layouts (D162); nothing is versioned by hand.

use blaze_format_core::{Own, Part, Placed, Record, SchemaHash, Slice, Writer, WriterOpts, schema_of};
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_ids::{ContentId, Hasher};
use blaze_own::planted::{Planted, Plants};
use blaze_own::{AVec, Alloc, Vec};

use crate::model::{
    AbiRow, BaseRow, ConstRow, DeclKind, DeclsBuilder, EnumRow, FieldRow, FnRow, HeaderRow, MOST_DEPTH, MacroRow,
    ModelError, Name, NamespaceRow, RecordRow, TypeId, TypeRow, TypedefRow, UnexpressedRow, UnitRow, VarRow,
    tag,
};
use crate::{NONE, at, narrow};

blaze_format_core::record! {
    /// The declaration record's root: every column (see [`crate::model`] for each row).
    #[derive(Clone, Copy, Debug)]
    pub struct DeclsRoot: 1 {
        pub name_bytes: Slice<u8>,
        pub name_ends: Slice<u32>,
        pub headers: Slice<HeaderRow>,
        pub namespaces: Slice<NamespaceRow>,
        pub types: Slice<TypeRow>,
        pub type_lists: Slice<u32>,
        pub records: Slice<RecordRow>,
        pub fields: Slice<FieldRow>,
        pub units: Slice<UnitRow>,
        pub bases: Slice<BaseRow>,
        pub enums: Slice<EnumRow>,
        pub consts: Slice<ConstRow>,
        pub fns: Slice<FnRow>,
        pub name_lists: Slice<u32>,
        pub vars: Slice<VarRow>,
        pub typedefs: Slice<TypedefRow>,
        pub macros: Slice<MacroRow>,
        pub strings: Slice<u8>,
        pub unexpressed: Slice<UnexpressedRow>,
        pub abi: AbiRow,
    }
}

/// The declaration record's schema (D162: derived from the rows).
pub fn decls_schema() -> SchemaHash {
    schema_of(&[Part::Str("blaze-c decls"), Part::Schema(<DeclsRoot as Record>::SCHEMA)])
}

/// The bugs a control can plant in [`encode`] (D165): each makes two equal models write unequal
/// bytes, which the determinism property must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CanonPlant {
    None,
    /// Functions in the order the front end made them, not sorted.
    FunctionsInBuilderOrder,
    /// Each use of a type a row of its own: equal types are not shared.
    TypesNotShared,
}

impl Plants for CanonPlant {}

/// A type row's identity while the record is written: the row with its references already
/// canonical, and for a function type its parameters' digest (the row's own list start is not
/// part of what it is).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TypeKey {
    row: TypeRow,
    params: ContentId,
}

struct Canon<'m, 'd, 'a, P> {
    model: &'m DeclsBuilder<'d>,
    plant: P,
    name_map: AVec<'a, u32>,
    header_map: AVec<'a, u32>,
    ns_map: AVec<'a, u32>,
    rec_map: AVec<'a, u32>,
    enum_map: AVec<'a, u32>,
    typedef_map: AVec<'a, u32>,
    type_memo: AVec<'a, u32>,
    type_ids: Map<TypeKey, u32, FixedState, Alloc<'a>>,
    /// Old record ids in their new order; the named ones first.
    rec_order: AVec<'a, u32>,
    rec_walked: usize,
    types: AVec<'a, TypeRow>,
    type_lists: AVec<'a, u32>,
    alloc: Alloc<'a>,
}

fn too_large(what: &'static str) -> ModelError {
    ModelError::TooLarge { what }
}

fn count(len: usize, what: &'static str) -> Result<u32, ModelError> {
    narrow(len).ok_or(too_large(what))
}

/// `NONE` stays `NONE`; anything else through `map`.
fn opt(map: &[u32], old: u32) -> u32 {
    match old {
        NONE => NONE,
        old => map[at(old)],
    }
}

impl<'m, 'd, 'a, P: Planted<CanonPlant>> Canon<'m, 'd, 'a, P> {
    /// The new number of record `old`, numbering an anonymous one now (first reach).
    fn record(&mut self, old: u32) -> Result<u32, ModelError> {
        if self.rec_map[at(old)] == NONE {
            let new = count(self.rec_order.len(), "records")?;
            self.rec_map[at(old)] = new;
            self.rec_order.push(old);
        }
        Ok(self.rec_map[at(old)])
    }

    fn ty(&mut self, old: u32, depth: u32) -> Result<u32, ModelError> {
        if depth > MOST_DEPTH {
            return Err(ModelError::TooDeep { ty: TypeId(old) });
        }
        if self.type_memo[at(old)] != NONE && !self.plant.has(CanonPlant::TypesNotShared) {
            return Ok(self.type_memo[at(old)]);
        }
        let mut row = self.model.types[at(old)];
        let mut params = ContentId::default();
        let mut list: AVec<'a, u32> = Vec::new_in(self.alloc);
        match row.kind {
            tag::POINTER | tag::REFERENCE | tag::ARRAY | tag::VA_LIST => row.a = self.ty(row.a, depth + 1)?,
            tag::FUNCTION => {
                row.a = self.ty(row.a, depth + 1)?;
                let model = self.model;
                for k in at(row.b)..at(row.b) + at(row.c) {
                    let param = self.ty(model.type_lists[k], depth + 1)?;
                    list.push(param);
                }
                let mut h = Hasher::new("blaze-c parameter list");
                for param in list.iter() {
                    h.update(&param.to_le_bytes());
                }
                params = h.finish();
                row.b = 0;
            }
            tag::RECORD => row.a = self.record(row.a)?,
            tag::ENUM => row.a = self.enum_map[at(row.a)],
            tag::TYPEDEF => row.a = self.typedef_map[at(row.a)],
            _ => {}
        }
        let key = TypeKey { row, params };
        let shared = match self.plant.has(CanonPlant::TypesNotShared) {
            true => None,
            false => self.type_ids.get(&key).copied(),
        };
        let new = match shared {
            Some(new) => new,
            None => {
                let new = count(self.types.len(), "types")?;
                if row.kind == tag::FUNCTION {
                    row.b = count(self.type_lists.len(), "parameter lists")?;
                    self.type_lists.extend_from_slice(list.as_slice());
                    count(self.type_lists.len(), "parameter lists")?;
                }
                self.types.push(row);
                self.type_ids.insert(key, new);
                new
            }
        };
        self.type_memo[at(old)] = new;
        Ok(new)
    }

    /// Walks the fields and bases of every record numbered and not yet walked.
    fn drain(&mut self) -> Result<(), ModelError> {
        while self.rec_walked < self.rec_order.len() {
            let old = self.rec_order[self.rec_walked];
            self.rec_walked += 1;
            let row = self.model.records[at(old)];
            let model = self.model;
            for field in &model.fields[at(row.fields_start)..at(row.fields_start) + at(row.fields_len)] {
                self.ty(field.ty, 0)?;
            }
            for base in &model.bases[at(row.bases_start)..at(row.bases_start) + at(row.bases_len)] {
                self.record(base.record)?;
            }
        }
        Ok(())
    }
}

/// `items` (old indices `0..len`) sorted by `key`, refused where two keys are equal: the
/// declarations a canonical order must tell apart.
fn sorted<'a, K: Ord>(
    len: usize,
    alloc: Alloc<'a>,
    key: impl Fn(usize) -> K,
    duplicate: impl Fn(usize) -> ModelError,
) -> Result<AVec<'a, u32>, ModelError> {
    let mut order: AVec<'a, u32> = Vec::with_capacity_in(len, alloc);
    for k in 0..len {
        order.push(count(k, "declarations")?);
    }
    order.sort_unstable_by(|x, y| key(at(*x)).cmp(&key(at(*y))));
    for pair in order.windows(2) {
        if key(at(pair[0])) == key(at(pair[1])) {
            return Err(duplicate(at(pair[1])));
        }
    }
    Ok(order)
}

/// The record of `model`, in `alloc`. `plant` is [`blaze_own::planted::NoPlant`] but in a
/// control.
pub fn encode<'b>(model: &DeclsBuilder<'_>, alloc: Alloc<'b>, plant: impl Planted<CanonPlant>) -> Result<AVec<'b, u8>, ModelError> {
    let names = model.name_ends.len();
    // Names: sorted by text, each once.
    let mut name_order: AVec<'b, u32> = Vec::with_capacity_in(names, alloc);
    for k in 0..names {
        name_order.push(count(k, "names")?);
    }
    name_order.sort_unstable_by(|x, y| model.text(Name(*x)).cmp(model.text(Name(*y))));
    let mut name_map: AVec<'b, u32> = Vec::with_capacity_in(names, alloc);
    name_map.resize(names, NONE);
    let mut name_bytes: AVec<'b, u8> = Vec::new_in(alloc);
    let mut name_ends: AVec<'b, u32> = Vec::new_in(alloc);
    let mut last: Option<&str> = None;
    for old in name_order.iter() {
        let text = model.text(Name(*old));
        if last != Some(text) {
            name_bytes.extend_from_slice(text.as_bytes());
            name_ends.push(count(name_bytes.len(), "name bytes")?);
            last = Some(text);
        }
        name_map[at(*old)] = count(name_ends.len() - 1, "names")?;
    }

    // Headers: sorted by digest and root, each once.
    let mut header_order: AVec<'b, u32> = Vec::new_in(alloc);
    for k in 0..model.headers.len() {
        header_order.push(count(k, "headers")?);
    }
    header_order.sort_unstable_by_key(|h| model.headers[at(*h)]);
    let mut header_map: AVec<'b, u32> = Vec::with_capacity_in(model.headers.len(), alloc);
    header_map.resize(model.headers.len(), NONE);
    let mut headers: AVec<'b, HeaderRow> = Vec::new_in(alloc);
    for old in header_order.iter() {
        let row = model.headers[at(*old)];
        if headers.last() != Some(&row) {
            headers.push(row);
        }
        header_map[at(*old)] = count(headers.len() - 1, "headers")?;
    }

    // Namespaces: by depth (a parent is made before its children), then by new parent and name.
    let ns_len = model.namespaces.len();
    let mut depth: AVec<'b, u32> = Vec::with_capacity_in(ns_len, alloc);
    for row in model.namespaces.iter() {
        let d = match row.parent {
            NONE => 0,
            parent => depth[at(parent)] + 1,
        };
        if d > MOST_DEPTH {
            return Err(ModelError::TooDeep { ty: TypeId(NONE) });
        }
        depth.push(d);
    }
    let mut ns_map: AVec<'b, u32> = Vec::with_capacity_in(ns_len, alloc);
    ns_map.resize(ns_len, NONE);
    let mut namespaces: AVec<'b, NamespaceRow> = Vec::new_in(alloc);
    let mut by_depth: AVec<'b, u32> = Vec::new_in(alloc);
    for k in 0..ns_len {
        by_depth.push(count(k, "namespaces")?);
    }
    by_depth.sort_unstable_by_key(|k| depth[at(*k)]);
    let mut start = 0;
    while start < by_depth.len() {
        let d = depth[at(by_depth[start])];
        let end = start + by_depth[start..].iter().take_while(|k| depth[at(**k)] == d).count();
        let level = &mut by_depth[start..end];
        {
            let (parents, names) = (&ns_map, &name_map);
            level.sort_unstable_by_key(|k| {
                let row = model.namespaces[at(*k)];
                (opt(parents, row.parent), names[at(row.name)])
            });
        }
        for k in level.iter() {
            // A parent is one level up, so numbered already.
            let row = model.namespaces[at(*k)];
            let (parent, name) = (opt(&ns_map, row.parent), name_map[at(row.name)]);
            let new = match namespaces.last() {
                Some(last) if last.parent == parent && last.name == name => count(namespaces.len() - 1, "namespaces")?,
                _ => {
                    namespaces.push(NamespaceRow { name, parent });
                    count(namespaces.len() - 1, "namespaces")?
                }
            };
            ns_map[at(*k)] = new;
        }
        start = end;
    }
    let ns = |old: u32| opt(&ns_map, old);
    let nm = |old: u32| opt(&name_map, old);

    // Named records, enums (anonymous ones by their first constant), typedefs.
    let named_records: AVec<'b, u32> = {
        let mut named: AVec<'b, u32> = Vec::new_in(alloc);
        for (k, row) in model.records.iter().enumerate() {
            if row.name != NONE {
                named.push(count(k, "records")?);
            }
        }
        let key = |k: u32| {
            let row = model.records[at(k)];
            (ns(row.ns), nm(row.name))
        };
        named.sort_unstable_by_key(|k| key(*k));
        for pair in named.windows(2) {
            if key(pair[0]) == key(pair[1]) {
                return Err(ModelError::Duplicate { kind: DeclKind::Record, name: Name(model.records[at(pair[1])].name) });
            }
        }
        named
    };
    let enum_key = |k: usize| {
        let row = model.enums[k];
        let first = match row.consts_len {
            0 => NONE,
            _ => nm(model.consts[at(row.consts_start)].name),
        };
        (ns(row.ns), nm(row.name), first)
    };
    let enum_order = sorted(model.enums.len(), alloc, enum_key, |k| ModelError::Duplicate {
        kind: DeclKind::Enum,
        name: Name(model.enums[k].name),
    })?;
    let typedef_key = |k: usize| {
        let row = model.typedefs[k];
        (ns(row.ns), nm(row.name))
    };
    let typedef_order = sorted(model.typedefs.len(), alloc, typedef_key, |k| ModelError::Duplicate {
        kind: DeclKind::Typedef,
        name: Name(model.typedefs[k].name),
    })?;
    let mut enum_map: AVec<'b, u32> = Vec::with_capacity_in(model.enums.len(), alloc);
    enum_map.resize(model.enums.len(), NONE);
    for (new, old) in enum_order.iter().enumerate() {
        enum_map[at(*old)] = count(new, "enums")?;
    }
    let mut typedef_map: AVec<'b, u32> = Vec::with_capacity_in(model.typedefs.len(), alloc);
    typedef_map.resize(model.typedefs.len(), NONE);
    for (new, old) in typedef_order.iter().enumerate() {
        typedef_map[at(*old)] = count(new, "typedefs")?;
    }
    let fn_key = |k: usize| {
        let row = model.fns[k];
        (ns(row.ns), nm(row.name), nm(row.symbol))
    };
    // Rows of one function (one key) are its declarations and at most one definition: sorted
    // with the definition first, then by place, so the row kept below (the first) does not
    // depend on the order the front end gave them in.
    let fn_place = |k: usize| {
        let row = model.fns[k];
        (fn_key(k), core::cmp::Reverse(row.body), opt(&header_map, row.header), row.at, row.len)
    };
    let fn_order = {
        let mut order: AVec<'b, u32> = Vec::new_in(alloc);
        for k in 0..model.fns.len() {
            order.push(count(k, "functions")?);
        }
        if !plant.has(CanonPlant::FunctionsInBuilderOrder) {
            order.sort_unstable_by_key(|k| fn_place(at(*k)));
        }
        order
    };
    // Each row's key, by its builder row, for the merge below (the name maps move into the walk).
    let mut fn_keys: AVec<'b, (u32, u32, u32)> = Vec::with_capacity_in(model.fns.len(), alloc);
    for k in 0..model.fns.len() {
        fn_keys.push(fn_key(k));
    }
    let var_order = sorted(
        model.vars.len(),
        alloc,
        |k| (ns(model.vars[k].ns), nm(model.vars[k].name)),
        |k| ModelError::Duplicate { kind: DeclKind::Variable, name: Name(model.vars[k].name) },
    )?;
    let macro_order = sorted(
        model.macros.len(),
        alloc,
        |k| nm(model.macros[k].name),
        |k| ModelError::Duplicate { kind: DeclKind::Macro, name: Name(model.macros[k].name) },
    )?;
    let unexpressed_key = |k: usize| {
        let row = model.unexpressed[k];
        (ns(row.ns), nm(row.name), row.kind, row.cause, opt(&header_map, row.header), row.at, row.len)
    };
    let mut unexpressed_order: AVec<'b, u32> = Vec::new_in(alloc);
    for k in 0..model.unexpressed.len() {
        unexpressed_order.push(count(k, "unexpressed declarations")?);
    }
    unexpressed_order.sort_unstable_by_key(|k| unexpressed_key(at(*k)));
    unexpressed_order.dedup_by_key(|k| unexpressed_key(at(*k)));

    let mut rec_map: AVec<'b, u32> = Vec::with_capacity_in(model.records.len(), alloc);
    rec_map.resize(model.records.len(), NONE);
    let mut rec_order: AVec<'b, u32> = Vec::with_capacity_in(model.records.len(), alloc);
    for (new, old) in named_records.iter().enumerate() {
        rec_map[at(*old)] = count(new, "records")?;
        rec_order.push(*old);
    }
    let mut type_memo: AVec<'b, u32> = Vec::with_capacity_in(model.types.len(), alloc);
    type_memo.resize(model.types.len(), NONE);
    let mut c = Canon {
        model,
        plant,
        name_map,
        header_map,
        ns_map,
        rec_map,
        enum_map,
        typedef_map,
        type_memo,
        type_ids: Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(model.types.len()) }),
        rec_order,
        rec_walked: 0,
        types: Vec::new_in(alloc),
        type_lists: Vec::new_in(alloc),
        alloc,
    };
    // The walk that numbers types and anonymous records.
    c.drain()?;
    let mut enum_types: AVec<'b, u32> = Vec::with_capacity_in(enum_order.len(), alloc);
    for old in enum_order.iter() {
        let row = model.enums[at(*old)];
        enum_types.push(match row.underlying {
            NONE => NONE,
            ty => c.ty(ty, 0)?,
        });
    }
    c.drain()?;
    let mut typedef_types: AVec<'b, u32> = Vec::with_capacity_in(typedef_order.len(), alloc);
    for old in typedef_order.iter() {
        typedef_types.push(c.ty(model.typedefs[at(*old)].ty, 0)?);
        c.drain()?;
    }
    // One row a function: a declaration repeated (C allows any number of compatible ones) or
    // given before its definition (a `static inline` forward declaration whose body is in a
    // later header) is the one row, the definition's. Rows of one key are merged only when
    // their types (as numbered here: structurally equal is equal) and linkages agree and at
    // most one has a body; anything else is two declarations, refused.
    let mut fn_kept: AVec<'b, u32> = Vec::with_capacity_in(fn_order.len(), alloc);
    let mut fn_types: AVec<'b, u32> = Vec::with_capacity_in(fn_order.len(), alloc);
    for old in fn_order.iter() {
        let row = model.fns[at(*old)];
        let ty = c.ty(row.ty, 0)?;
        if row.owner != NONE {
            c.record(row.owner)?;
        }
        c.drain()?;
        if let (Some(&prev), Some(&prev_ty)) = (fn_kept.last(), fn_types.last())
            && fn_keys[at(prev)] == fn_keys[at(*old)]
        {
            let first = model.fns[at(prev)];
            let agree = prev_ty == ty && first.linkage == row.linkage && first.method == row.method && first.owner == row.owner;
            // Sorted definition first: a second body is a second row with `body` 1.
            if !agree || row.body == 1 {
                return Err(ModelError::Redeclared { name: Name(row.name) });
            }
            continue;
        }
        fn_kept.push(*old);
        fn_types.push(ty);
    }
    let fn_order = fn_kept;
    let mut var_types: AVec<'b, u32> = Vec::with_capacity_in(var_order.len(), alloc);
    for old in var_order.iter() {
        var_types.push(c.ty(model.vars[at(*old)].ty, 0)?);
        c.drain()?;
    }
    let mut macro_types: AVec<'b, u32> = Vec::with_capacity_in(macro_order.len(), alloc);
    for old in macro_order.iter() {
        macro_types.push(c.ty(model.macros[at(*old)].ty, 0)?);
    }

    // The rows, in their new order, with every reference new.
    let hd = |old: u32| opt(&c.header_map, old);
    let mut records: AVec<'b, RecordRow> = Vec::with_capacity_in(c.rec_order.len(), alloc);
    let mut fields: AVec<'b, FieldRow> = Vec::new_in(alloc);
    let mut units: AVec<'b, UnitRow> = Vec::new_in(alloc);
    let mut bases: AVec<'b, BaseRow> = Vec::new_in(alloc);
    for old in c.rec_order.iter() {
        let mut row = model.records[at(*old)];
        let fields_start = count(fields.len(), "fields")?;
        let units_start = count(units.len(), "units")?;
        let bases_start = count(bases.len(), "bases")?;
        for field in &model.fields[at(row.fields_start)..at(row.fields_start) + at(row.fields_len)] {
            let mut f = *field;
            f.name = opt(&c.name_map, f.name);
            f.ty = c.type_memo[at(f.ty)];
            if f.unit != NONE {
                f.unit = units_start + (f.unit - row.units_start);
            }
            fields.push(f);
        }
        units.extend_from_slice(&model.units[at(row.units_start)..at(row.units_start) + at(row.units_len)]);
        for base in &model.bases[at(row.bases_start)..at(row.bases_start) + at(row.bases_len)] {
            bases.push(BaseRow { record: c.rec_map[at(base.record)], ..*base });
        }
        row.name = opt(&c.name_map, row.name);
        row.ns = opt(&c.ns_map, row.ns);
        row.header = hd(row.header);
        row.fields_start = fields_start;
        row.units_start = units_start;
        row.bases_start = bases_start;
        records.push(row);
    }
    let mut enums: AVec<'b, EnumRow> = Vec::with_capacity_in(enum_order.len(), alloc);
    let mut consts: AVec<'b, ConstRow> = Vec::new_in(alloc);
    for (k, old) in enum_order.iter().enumerate() {
        let mut row = model.enums[at(*old)];
        let start = count(consts.len(), "enum constants")?;
        for cst in &model.consts[at(row.consts_start)..at(row.consts_start) + at(row.consts_len)] {
            consts.push(ConstRow { name: opt(&c.name_map, cst.name), ..*cst });
        }
        row.name = opt(&c.name_map, row.name);
        row.ns = opt(&c.ns_map, row.ns);
        row.header = hd(row.header);
        row.underlying = enum_types[k];
        row.consts_start = start;
        enums.push(row);
    }
    let mut typedefs: AVec<'b, TypedefRow> = Vec::with_capacity_in(typedef_order.len(), alloc);
    for (k, old) in typedef_order.iter().enumerate() {
        let row = model.typedefs[at(*old)];
        typedefs.push(TypedefRow {
            name: opt(&c.name_map, row.name),
            ns: opt(&c.ns_map, row.ns),
            ty: typedef_types[k],
            header: hd(row.header),
            ..row
        });
    }
    let mut fns: AVec<'b, FnRow> = Vec::with_capacity_in(fn_order.len(), alloc);
    let mut name_lists: AVec<'b, u32> = Vec::new_in(alloc);
    for (k, old) in fn_order.iter().enumerate() {
        let row = model.fns[at(*old)];
        let start = count(name_lists.len(), "parameter names")?;
        for name in &model.name_lists[at(row.params_start)..at(row.params_start) + at(row.params_len)] {
            name_lists.push(opt(&c.name_map, *name));
        }
        fns.push(FnRow {
            name: opt(&c.name_map, row.name),
            ns: opt(&c.ns_map, row.ns),
            symbol: opt(&c.name_map, row.symbol),
            ty: fn_types[k],
            owner: opt(&c.rec_map, row.owner),
            params_start: start,
            header: hd(row.header),
            ..row
        });
    }
    let mut vars: AVec<'b, VarRow> = Vec::with_capacity_in(var_order.len(), alloc);
    for (k, old) in var_order.iter().enumerate() {
        let row = model.vars[at(*old)];
        vars.push(VarRow {
            name: opt(&c.name_map, row.name),
            ns: opt(&c.ns_map, row.ns),
            symbol: opt(&c.name_map, row.symbol),
            ty: var_types[k],
            header: hd(row.header),
            ..row
        });
    }
    // A slot's table and an address's variable by their new rows.
    let mut var_map: AVec<'b, u32> = Vec::with_capacity_in(model.vars.len(), alloc);
    var_map.resize(model.vars.len(), NONE);
    for (new, old) in var_order.iter().enumerate() {
        var_map[at(*old)] = count(new, "variables")?;
    }
    let mut macros: AVec<'b, MacroRow> = Vec::with_capacity_in(macro_order.len(), alloc);
    let mut strings: AVec<'b, u8> = Vec::new_in(alloc);
    for (k, old) in macro_order.iter().enumerate() {
        let row = model.macros[at(*old)];
        let str_start = count(strings.len(), "string bytes")?;
        strings.extend_from_slice(&model.strings[at(row.str_start)..at(row.str_start) + at(row.str_len)]);
        macros.push(MacroRow {
            name: opt(&c.name_map, row.name),
            ty: macro_types[k],
            var: opt(&var_map, row.var),
            str_start: if row.str_len == 0 { 0 } else { str_start },
            header: hd(row.header),
            ..row
        });
    }
    let mut unexpressed: AVec<'b, UnexpressedRow> = Vec::with_capacity_in(unexpressed_order.len(), alloc);
    for old in unexpressed_order.iter() {
        let row = model.unexpressed[at(*old)];
        unexpressed.push(UnexpressedRow {
            name: opt(&c.name_map, row.name),
            ns: opt(&c.ns_map, row.ns),
            header: hd(row.header),
            ..row
        });
    }

    let mut out = Writer::new(WriterOpts { schema: decls_schema(), alloc: Own(alloc), capacity: None });
    let p_name_bytes = Placed::push(&mut out, name_bytes.as_slice());
    let p_name_ends = Placed::push(&mut out, name_ends.as_slice());
    let p_headers = Placed::push(&mut out, headers.as_slice());
    let p_namespaces = Placed::push(&mut out, namespaces.as_slice());
    let p_types = Placed::push(&mut out, c.types.as_slice());
    let p_type_lists = Placed::push(&mut out, c.type_lists.as_slice());
    let p_records = Placed::push(&mut out, records.as_slice());
    let p_fields = Placed::push(&mut out, fields.as_slice());
    let p_units = Placed::push(&mut out, units.as_slice());
    let p_bases = Placed::push(&mut out, bases.as_slice());
    let p_enums = Placed::push(&mut out, enums.as_slice());
    let p_consts = Placed::push(&mut out, consts.as_slice());
    let p_fns = Placed::push(&mut out, fns.as_slice());
    let p_name_lists = Placed::push(&mut out, name_lists.as_slice());
    let p_vars = Placed::push(&mut out, vars.as_slice());
    let p_typedefs = Placed::push(&mut out, typedefs.as_slice());
    let p_macros = Placed::push(&mut out, macros.as_slice());
    let p_strings = Placed::push(&mut out, strings.as_slice());
    let p_unexpressed = Placed::push(&mut out, unexpressed.as_slice());
    let root_at = out.next::<DeclsRoot>();
    let f = |offset: usize| root_at + u64::try_from(offset).expect("an offset fits u64");
    use core::mem::offset_of;
    let root = DeclsRoot {
        name_bytes: p_name_bytes.slice(f(offset_of!(DeclsRoot, name_bytes))),
        name_ends: p_name_ends.slice(f(offset_of!(DeclsRoot, name_ends))),
        headers: p_headers.slice(f(offset_of!(DeclsRoot, headers))),
        namespaces: p_namespaces.slice(f(offset_of!(DeclsRoot, namespaces))),
        types: p_types.slice(f(offset_of!(DeclsRoot, types))),
        type_lists: p_type_lists.slice(f(offset_of!(DeclsRoot, type_lists))),
        records: p_records.slice(f(offset_of!(DeclsRoot, records))),
        fields: p_fields.slice(f(offset_of!(DeclsRoot, fields))),
        units: p_units.slice(f(offset_of!(DeclsRoot, units))),
        bases: p_bases.slice(f(offset_of!(DeclsRoot, bases))),
        enums: p_enums.slice(f(offset_of!(DeclsRoot, enums))),
        consts: p_consts.slice(f(offset_of!(DeclsRoot, consts))),
        fns: p_fns.slice(f(offset_of!(DeclsRoot, fns))),
        name_lists: p_name_lists.slice(f(offset_of!(DeclsRoot, name_lists))),
        vars: p_vars.slice(f(offset_of!(DeclsRoot, vars))),
        typedefs: p_typedefs.slice(f(offset_of!(DeclsRoot, typedefs))),
        macros: p_macros.slice(f(offset_of!(DeclsRoot, macros))),
        strings: p_strings.slice(f(offset_of!(DeclsRoot, strings))),
        unexpressed: p_unexpressed.slice(f(offset_of!(DeclsRoot, unexpressed))),
        abi: model.abi,
    };
    let root = out.push(&root);
    Ok(out.finish(root))
}
