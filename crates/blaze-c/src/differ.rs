//! The differential compare of two declaration records: the first difference as a typed value
//! naming the table, the declaration, the part (field, constant, parameter) and the column.
//! Two front ends (the real one and a reference), two toolchains, two targets: their records
//! for one request differ where [`first_difference`] says, or nowhere.
//!
//! The order of the walk is the record's: the target's facts, the headers, the namespaces, then
//! each declaration table in its canonical order, a declaration's own columns before its parts,
//! the meaning of a row before its place in the header; the shared types last, since a type
//! that differs shows first as the declaration that uses it. Names are compared as text, so a
//! name added to one record does not shift every later comparison; rows that point at other
//! rows (a type, a namespace) compare by row, which the canonical numbering makes equal for
//! equal models.

use blaze_own::planted::{Planted, Plants};

use crate::view::{DeclsView, Table};
use crate::{NONE, narrow};

/// The bugs a control can plant in [`first_difference`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DifferPlant {
    None,
    /// A field's offset is not compared.
    IgnoresFieldOffsets,
    /// The shared type rows are not compared.
    IgnoresTypes,
    /// A macro's value is not compared.
    IgnoresMacroValues,
}

impl Plants for DifferPlant {}

/// The column a difference is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    /// The number of rows of the table, or of a declaration's parts.
    Count,
    Name,
    Namespace,
    Parent,
    Path,
    Root,
    Kind,
    Size,
    Align,
    Pack,
    Complete,
    Relocation,
    Offset,
    Type,
    Width,
    Bitfield,
    Unit,
    Base,
    Underlying,
    Scoped,
    Value,
    Symbol,
    Owner,
    Method,
    Attrs,
    Text,
    Cause,
    Quals,
    Flags,
    Extra,
    /// A type row's operands (what they mean is by its kind).
    Operand,
    /// Where the declaration is in its header: the header, the offset or the length.
    Position,
    PointerSize,
    PointerAlign,
    Endian,
    CharSigned,
    /// A function's or variable's [`crate::model::Linkage`].
    Linkage,
    /// Whether a function's body is in the headers.
    Body,
    /// A variable's initializer (its kind and bits).
    Init,
    /// The variable a slot or address macro names.
    Variable,
}

/// A part of a declaration: its index among the declaration's fields, constants, units, bases
/// or parameters, and its name when it has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Part<'b> {
    pub index: u32,
    pub name: Option<&'b str>,
}

/// Where two records first differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Difference<'b> {
    pub table: Table,
    /// The row in the first record ([`NONE`] for the target's facts).
    pub row: u32,
    /// The declaration's name, from the first record (from the second when the first has none).
    pub decl: Option<&'b str>,
    pub part: Option<Part<'b>>,
    pub column: Column,
}

struct At<'b> {
    table: Table,
    row: u32,
    decl: Option<&'b str>,
    part: Option<Part<'b>>,
}

impl<'b> At<'b> {
    fn at(&self, column: Column) -> Difference<'b> {
        Difference { table: self.table, row: self.row, decl: self.decl, part: self.part, column }
    }

    fn part(&self, index: usize, name: Option<&'b str>) -> At<'b> {
        At { table: self.table, row: self.row, decl: self.decl, part: Some(Part { index: narrow(index).unwrap_or(NONE), name }) }
    }
}

/// `return Some(difference)` when the two values differ.
macro_rules! same {
    ($at:expr, $column:expr, $x:expr, $y:expr) => {
        if $x != $y {
            return Some($at.at($column));
        }
    };
}

/// The first difference of `a` from `b`, `None` when they hold the same declarations.
pub fn first_difference<'b>(a: &DeclsView<'b>, b: &DeclsView<'b>, plant: impl Planted<DifferPlant>) -> Option<Difference<'b>> {
    let row = |table: Table, k: usize, name_a: u32, name_b: u32| At {
        table,
        row: narrow(k).unwrap_or(NONE),
        decl: a.opt_name(name_a).or(b.opt_name(name_b)),
        part: None,
    };
    let count = |table: Table| At { table, row: NONE, decl: None, part: None };

    let abi = count(Table::Abi);
    same!(abi, Column::PointerSize, a.abi.pointer_size, b.abi.pointer_size);
    same!(abi, Column::PointerAlign, a.abi.pointer_align, b.abi.pointer_align);
    same!(abi, Column::Endian, a.abi.little_endian, b.abi.little_endian);
    same!(abi, Column::CharSigned, a.abi.char_signed, b.abi.char_signed);

    same!(count(Table::Headers), Column::Count, a.headers.len(), b.headers.len());
    for (k, (x, y)) in a.headers.iter().zip(b.headers).enumerate() {
        let at = row(Table::Headers, k, NONE, NONE);
        same!(at, Column::Root, x.root, y.root);
        same!(at, Column::Path, x.path, y.path);
    }

    same!(count(Table::Namespaces), Column::Count, a.namespaces.len(), b.namespaces.len());
    for (k, (x, y)) in a.namespaces.iter().zip(b.namespaces).enumerate() {
        let at = row(Table::Namespaces, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Parent, x.parent, y.parent);
    }

    same!(count(Table::Records), Column::Count, a.records.len(), b.records.len());
    for (k, (x, y)) in a.records.iter().zip(b.records).enumerate() {
        let at = row(Table::Records, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Kind, x.kind, y.kind);
        same!(at, Column::Complete, x.complete, y.complete);
        same!(at, Column::Size, x.size, y.size);
        same!(at, Column::Align, x.align, y.align);
        same!(at, Column::Pack, x.pack, y.pack);
        same!(at, Column::Relocation, x.relocation, y.relocation);
        let (fa, fb) = (a.fields_of(x), b.fields_of(y));
        same!(at, Column::Count, fa.len(), fb.len());
        for (j, (f, g)) in fa.iter().zip(fb).enumerate() {
            let part = at.part(j, a.opt_name(f.name).or(b.opt_name(g.name)));
            same!(part, Column::Name, a.opt_name(f.name), b.opt_name(g.name));
            same!(part, Column::Type, f.ty, g.ty);
            if !plant.has(DifferPlant::IgnoresFieldOffsets) {
                same!(part, Column::Offset, f.offset_bits, g.offset_bits);
            }
            same!(part, Column::Bitfield, f.bitfield, g.bitfield);
            same!(part, Column::Width, f.width, g.width);
            let unit = |u: u32, start: u32| if u == NONE { NONE } else { u.wrapping_sub(start) };
            same!(part, Column::Unit, unit(f.unit, x.units_start), unit(g.unit, y.units_start));
        }
        let (ua, ub) = (a.units_of(x), b.units_of(y));
        same!(at, Column::Count, ua.len(), ub.len());
        for (j, (u, w)) in ua.iter().zip(ub).enumerate() {
            let part = at.part(j, None);
            same!(part, Column::Offset, u.offset, w.offset);
            same!(part, Column::Size, u.size, w.size);
        }
        let (ba, bb) = (a.bases_of(x), b.bases_of(y));
        same!(at, Column::Count, ba.len(), bb.len());
        for (j, (u, w)) in ba.iter().zip(bb).enumerate() {
            let part = at.part(j, None);
            same!(part, Column::Base, u.record, w.record);
            same!(part, Column::Offset, u.offset, w.offset);
            same!(part, Column::Flags, u.flags, w.flags);
        }
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Enums), Column::Count, a.enums.len(), b.enums.len());
    for (k, (x, y)) in a.enums.iter().zip(b.enums).enumerate() {
        let at = row(Table::Enums, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Complete, x.complete, y.complete);
        same!(at, Column::Scoped, x.scoped, y.scoped);
        same!(at, Column::Underlying, x.underlying, y.underlying);
        let (ca, cb) = (a.consts_of(x), b.consts_of(y));
        same!(at, Column::Count, ca.len(), cb.len());
        for (j, (c, d)) in ca.iter().zip(cb).enumerate() {
            let part = at.part(j, a.opt_name(c.name));
            same!(part, Column::Name, a.opt_name(c.name), b.opt_name(d.name));
            same!(part, Column::Value, c.value, d.value);
            same!(part, Column::Position, (c.at, c.len), (d.at, d.len));
        }
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Typedefs), Column::Count, a.typedefs.len(), b.typedefs.len());
    for (k, (x, y)) in a.typedefs.iter().zip(b.typedefs).enumerate() {
        let at = row(Table::Typedefs, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Type, x.ty, y.ty);
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Fns), Column::Count, a.fns.len(), b.fns.len());
    for (k, (x, y)) in a.fns.iter().zip(b.fns).enumerate() {
        let at = row(Table::Fns, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Symbol, a.opt_name(x.symbol), b.opt_name(y.symbol));
        same!(at, Column::Type, x.ty, y.ty);
        same!(at, Column::Owner, x.owner, y.owner);
        same!(at, Column::Method, x.method, y.method);
        same!(at, Column::Attrs, x.attrs, y.attrs);
        same!(at, Column::Linkage, x.linkage, y.linkage);
        same!(at, Column::Body, x.body, y.body);
        let (pa, pb) = (a.param_names(x), b.param_names(y));
        same!(at, Column::Count, pa.len(), pb.len());
        for (j, (p, q)) in pa.iter().zip(pb).enumerate() {
            let part = at.part(j, a.opt_name(*p));
            same!(part, Column::Name, a.opt_name(*p), b.opt_name(*q));
        }
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Vars), Column::Count, a.vars.len(), b.vars.len());
    for (k, (x, y)) in a.vars.iter().zip(b.vars).enumerate() {
        let at = row(Table::Vars, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Symbol, a.opt_name(x.symbol), b.opt_name(y.symbol));
        same!(at, Column::Type, x.ty, y.ty);
        same!(at, Column::Attrs, x.attrs, y.attrs);
        same!(at, Column::Linkage, x.linkage, y.linkage);
        same!(at, Column::Init, (x.init, x.init_bits), (y.init, y.init_bits));
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Macros), Column::Count, a.macros.len(), b.macros.len());
    for (k, (x, y)) in a.macros.iter().zip(b.macros).enumerate() {
        let at = row(Table::Macros, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Kind, x.kind, y.kind);
        same!(at, Column::Type, x.ty, y.ty);
        same!(at, Column::Variable, x.var, y.var);
        same!(at, Column::Flags, x.deref, y.deref);
        if !plant.has(DifferPlant::IgnoresMacroValues) {
            same!(at, Column::Value, x.value, y.value);
            same!(at, Column::Text, a.string_of(x), b.string_of(y));
        }
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    same!(count(Table::Unexpressed), Column::Count, a.unexpressed.len(), b.unexpressed.len());
    for (k, (x, y)) in a.unexpressed.iter().zip(b.unexpressed).enumerate() {
        let at = row(Table::Unexpressed, k, x.name, y.name);
        same!(at, Column::Name, a.opt_name(x.name), b.opt_name(y.name));
        same!(at, Column::Namespace, x.ns, y.ns);
        same!(at, Column::Kind, x.kind, y.kind);
        same!(at, Column::Cause, x.cause, y.cause);
        same!(at, Column::Position, (x.header, x.at, x.len), (y.header, y.at, y.len));
    }

    if !plant.has(DifferPlant::IgnoresTypes) {
        same!(count(Table::Types), Column::Count, a.types.len(), b.types.len());
        for (k, (x, y)) in a.types.iter().zip(b.types).enumerate() {
            let at = row(Table::Types, k, NONE, NONE);
            same!(at, Column::Kind, x.kind, y.kind);
            same!(at, Column::Quals, x.quals, y.quals);
            same!(at, Column::Flags, x.flags, y.flags);
            same!(at, Column::Extra, x.extra, y.extra);
            // A function type's list start is where its parameters were written, not what it is.
            if x.kind == crate::model::tag::FUNCTION {
                same!(at, Column::Operand, x.a, y.a);
                let (pa, pb) = (a.params_of(x), b.params_of(y));
                same!(at, Column::Count, pa.len(), pb.len());
                for (j, (p, q)) in pa.iter().zip(pb).enumerate() {
                    same!(at.part(j, None), Column::Type, p, q);
                }
            } else {
                same!(at, Column::Operand, (x.a, x.b, x.c), (y.a, y.b, y.c));
            }
        }
    }
    None
}
