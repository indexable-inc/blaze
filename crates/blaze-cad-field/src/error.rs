//! Typed errors that name the thing (rule 19): the node kind, the author's name for the node
//! when one was given, the argument, its value and what is accepted. Nothing here is a string
//! built at run time; each error prints by `Show`.

use core::fmt;

use blaze_text::{Put, Show, Shown, Writes};
use blaze_units::{Angle, Length};

use crate::level::Level;

/// What kind of node an error is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    HalfSpace,
    Sphere,
    Box,
    Cylinder,
    Capsule,
    Cone,
    Torus,
    Ellipsoid,
    Superellipsoid,
    Gyroid,
    Profile,
    Extrude,
    Revolve,
    Loft,
    GuidedLoft,
    SweepArc,
    SweepPolyline,
    SweepFrames,
    Place,
    Scale,
    Affine,
    Mirror,
    LinearRepeat,
    CircularRepeat,
    Union,
    Intersect,
    Difference,
    SmoothBoolean,
    ChamferBoolean,
    VariableSmoothBoolean,
    VariableChamferBoolean,
    Grow,
    Erode,
    VariableGrow,
    VariableErode,
    Shell,
    RoundEdges,
    Twist,
    Bend,
    Taper,
    Detail,
    Redistance,
    Empty,
}

impl NodeKind {
    /// The kind as a person writes it.
    pub const fn word(self) -> &'static str {
        match self {
            NodeKind::HalfSpace => "half-space",
            NodeKind::Sphere => "sphere",
            NodeKind::Box => "box",
            NodeKind::Cylinder => "cylinder",
            NodeKind::Capsule => "capsule",
            NodeKind::Cone => "cone",
            NodeKind::Torus => "torus",
            NodeKind::Ellipsoid => "ellipsoid",
            NodeKind::Superellipsoid => "superellipsoid",
            NodeKind::Gyroid => "gyroid",
            NodeKind::Profile => "profile",
            NodeKind::Extrude => "extrude",
            NodeKind::Revolve => "revolve",
            NodeKind::Loft => "loft",
            NodeKind::GuidedLoft => "guided loft",
            NodeKind::SweepArc => "sweep along an arc",
            NodeKind::SweepPolyline => "sweep along a polyline",
            NodeKind::SweepFrames => "sweep along frames",
            NodeKind::Place => "placement",
            NodeKind::Scale => "uniform scale",
            NodeKind::Affine => "affine map",
            NodeKind::Mirror => "mirrored copy",
            NodeKind::LinearRepeat => "linear repeat",
            NodeKind::CircularRepeat => "circular repeat",
            NodeKind::Union => "union",
            NodeKind::Intersect => "intersection",
            NodeKind::Difference => "difference",
            NodeKind::SmoothBoolean => "rounded boolean",
            NodeKind::ChamferBoolean => "chamfered boolean",
            NodeKind::VariableSmoothBoolean => "variable rounded boolean",
            NodeKind::VariableChamferBoolean => "variable chamfered boolean",
            NodeKind::Grow => "grow",
            NodeKind::Erode => "erode",
            NodeKind::VariableGrow => "variable grow",
            NodeKind::VariableErode => "variable erode",
            NodeKind::Shell => "shell",
            NodeKind::RoundEdges => "round all edges",
            NodeKind::Twist => "twist",
            NodeKind::Bend => "bend",
            NodeKind::Taper => "taper",
            NodeKind::Detail => "detail",
            NodeKind::Redistance => "redistance",
            NodeKind::Empty => "empty",
        }
    }
}

/// An author's name for a node, held inline (at most [`Name::CAP`] bytes; a longer name is cut
/// at a character boundary and ends in `...`). Names are never part of a digest.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; Name::CAP],
    len: u8,
}

impl Name {
    /// The most bytes kept.
    pub const CAP: usize = 48;

    /// `text`, cut to fit.
    pub fn new(text: &str) -> Name {
        let mut bytes = [0u8; Name::CAP];
        let mut end = text.len();
        let cut = end > Name::CAP;
        if cut {
            end = Name::CAP - 3;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        let mut len = end;
        if cut {
            bytes[len..len + 3].copy_from_slice(b"...");
            len += 3;
        }
        // `len <= CAP = 48`, which fits a byte.
        Name { bytes, len: len as u8 }
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        // Built from a `str` cut at a character boundary and ASCII dots: always UTF-8.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// An argument's value, as it was given.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Length(Length),
    Angle(Angle),
    Count(u64),
    Ratio(f64),
    Level(Level),
}

impl Show for Value {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Value::Length(l) => {
                out.show(l);
            }
            Value::Angle(a) => {
                out.show(a);
            }
            Value::Count(n) => {
                out.int(*n);
            }
            Value::Ratio(r) => {
                out.float(*r);
            }
            Value::Level(l) => {
                out.str(l.name());
            }
        }
    }
}

/// Why building a node was refused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CadError {
    /// An argument outside what the node accepts.
    Argument { kind: NodeKind, name: Option<Name>, arg: &'static str, value: Value, accepted: &'static str },
    /// The node needs its operand to be bounded (a finite box), and it is not.
    Unbounded { kind: NodeKind, name: Option<Name>, what: &'static str },
    /// A repeat or mirrored copy whose copies would overlap: the fold that evaluates one copy
    /// is only right when they are apart.
    Overlap { kind: NodeKind, name: Option<Name>, extent: Length, room: Length },
    /// A table of the graph is full ([`crate::Limits`]).
    Full { what: &'static str, most: u64 },
    /// A node id that is not this graph's.
    Foreign { id: u32 },
}

impl Show for CadError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let named = |out: &mut P, kind: NodeKind, name: &Option<Name>| {
            out.str("the ").str(kind.word());
            if let Some(n) = name {
                out.str(" `").str(n.as_str()).str("`");
            }
        };
        match self {
            CadError::Argument { kind, name, arg, value, accepted } => {
                named(out, *kind, name);
                out.str(": ").str(arg).str(" is ").show(value).str("; accepted: ").str(accepted);
            }
            CadError::Unbounded { kind, name, what } => {
                named(out, *kind, name);
                out.str(": ").str(what).str(" is unbounded; a finite operand is needed");
            }
            CadError::Overlap { kind, name, extent, room } => {
                named(out, *kind, name);
                out.str(": the operand is ").show(extent).str(" across, and each copy has ").show(room);
                out.str("; the copies must not touch");
            }
            CadError::Full { what, most } => {
                out.str("the graph's ").str(what).str(" table is full at ").int(*most).str(" (Limits)");
            }
            CadError::Foreign { id } => {
                out.str("node ").int(*id).str(" is not in this graph");
            }
        }
    }
}

impl fmt::Display for CadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for CadError {}

/// Why a compile was refused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompileError {
    /// A redistance node with no solved grid attached ([`crate::Graph::attach_grid`]).
    Unsolved { name: Option<Name> },
    /// The tape would pass [`crate::Limits::most_instrs`].
    TooLong { most: u64 },
    /// More values live at once than registers ([`crate::Limits::most_registers`]).
    TooWide { most: u64 },
    /// A node id that is not this graph's.
    Foreign { id: u32 },
    /// Choices made for another tape: `given` sites where the tape has `sites`.
    WrongChoices { sites: u64, given: u64 },
}

impl Show for CompileError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            CompileError::Unsolved { name } => {
                out.str("the redistance node");
                if let Some(n) = name {
                    out.str(" `").str(n.as_str()).str("`");
                }
                out.str(" has no solved grid; the mesher attaches one");
            }
            CompileError::TooLong { most } => {
                out.str("the tape would pass ").int(*most).str(" instructions (Limits::most_instrs)");
            }
            CompileError::TooWide { most } => {
                out.str("more than ").int(*most).str(" values are live at once (Limits::most_registers)");
            }
            CompileError::Foreign { id } => {
                out.str("node ").int(*id).str(" is not in this graph");
            }
            CompileError::WrongChoices { sites, given } => {
                out.str("the choices have ").int(*given).str(" sites and the tape ").int(*sites).str("; choices are made by `eval_box` on the same tape");
            }
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for CompileError {}

/// Why a tape's bytes were refused by [`crate::Tape::decode`]. Every byte is checked before
/// the tape is used: the reader trusts nothing but the schema digest it was built with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// The bytes end at `have` where `need` are needed.
    Short { need: u64, have: u64 },
    /// The first four bytes are not `BCFT`.
    Magic,
    /// The tape was written under another schema (its op table or layout differ).
    Schema,
    /// Instruction `at` has the unknown op byte `byte`.
    Op { at: u64, byte: u8 },
    /// Instruction `at` reads value `operand`, which is not an earlier instruction.
    Operand { at: u64, operand: u32 },
    /// Instruction `at` reads constants past the pool.
    Pool { at: u64 },
    /// Instruction `at` names a table that is not there, or a table's rows pass the data.
    Table { at: u64 },
    /// The root is not an instruction.
    Root { root: u32, len: u64 },
    /// The level byte is not a level.
    Level { byte: u8 },
    /// A count passes [`crate::Limits`].
    TooLong { most: u64 },
    /// Bytes are left after the tape.
    Trailing { extra: u64 },
    /// More values are live at once than registers ([`crate::Limits::most_registers`]).
    TooWide { most: u64 },
}

impl Show for DecodeError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.str("tape bytes refused: ");
        match self {
            DecodeError::Short { need, have } => {
                out.str("they end at byte ").int(*have).str(" where ").int(*need).str(" are needed");
            }
            DecodeError::Magic => {
                out.str("they do not start with `BCFT`");
            }
            DecodeError::Schema => {
                out.str("they were written under another tape schema");
            }
            DecodeError::Op { at, byte } => {
                out.str("instruction ").int(*at).str(" has the unknown op ").int(u64::from(*byte));
            }
            DecodeError::Operand { at, operand } => {
                out.str("instruction ").int(*at).str(" reads value ").int(u64::from(*operand)).str(", which is not before it");
            }
            DecodeError::Pool { at } => {
                out.str("instruction ").int(*at).str(" reads constants past the pool");
            }
            DecodeError::Table { at } => {
                out.str("instruction or table ").int(*at).str(" names rows that are not there");
            }
            DecodeError::Root { root, len } => {
                out.str("the root ").int(u64::from(*root)).str(" is not one of the ").int(*len).str(" instructions");
            }
            DecodeError::Level { byte } => {
                out.str("the level byte ").int(u64::from(*byte)).str(" is not a level");
            }
            DecodeError::TooLong { most } => {
                out.str("a count passes ").int(*most).str(" (Limits)");
            }
            DecodeError::Trailing { extra } => {
                out.int(*extra).str(" bytes follow the tape");
            }
            DecodeError::TooWide { most } => {
                out.str("more than ").int(*most).str(" values are live at once (Limits::most_registers)");
            }
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for DecodeError {}

/// Why an evaluation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvalError {
    /// The coordinate and output slices differ in length.
    Lengths { x: u64, y: u64, z: u64, out: u64 },
    /// The coordinate, value, and gradient slices differ in length.
    GradLengths { x: u64, y: u64, z: u64, vals: u64, grads: u64 },
}

impl Show for EvalError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            EvalError::Lengths { x, y, z, out: o } => {
                out.str("eval_points: x has ").int(*x).str(" values, y ").int(*y).str(", z ").int(*z).str(" and out ").int(*o);
                out.str("; all four must be equal");
            }
            EvalError::GradLengths { x, y, z, vals, grads } => {
                out.str("eval_grads: x has ").int(*x).str(" values, y ").int(*y).str(", z ").int(*z);
                out.str(", vals ").int(*vals).str(" and grads ").int(*grads);
                out.str("; all five must be equal");
            }
        }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for EvalError {}

/// Why an inspection gave no answer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InspectError {
    /// The box or step budget ran out before the question was settled.
    Budget { what: &'static str, most: u64 },
    /// The tape's box is unbounded and no `within` box was given.
    Unbounded { what: &'static str },
    /// A ray's direction has no length.
    ZeroDirection,
    /// An argument outside what is accepted.
    Argument { arg: &'static str, value: f64, accepted: &'static str },
    /// Building a derived tape (a section) was refused.
    Compile(CompileError),
}

impl Show for InspectError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            InspectError::Budget { what, most } => {
                out.str(what).str(": the budget of ").int(*most).str(" ran out before the answer was settled");
            }
            InspectError::Unbounded { what } => {
                out.str(what).str(": the solid is unbounded; give a `within` box");
            }
            InspectError::ZeroDirection => {
                out.str("ray: the direction has no length");
            }
            InspectError::Argument { arg, value, accepted } => {
                out.str(arg).str(" is ").float(*value).str("; accepted: ").str(accepted);
            }
            InspectError::Compile(e) => {
                out.show(e);
            }
        }
    }
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for InspectError {}
