//! `cfront-decls 2`: the declarations the helper read from an import's headers, as text it
//! writes and this codec reads straight into blaze-c's [`DeclsBuilder`] (D170 C5: `feed(bytes)
//! -> Progress`, then `finish()`). One format (rule 30): every line is one builder call, in the
//! order the helper made them, so the model in crates/blaze-c/src/model.rs is the one statement
//! of what a front end gives and this is its spelling on the wire. It is blaze's own form,
//! versioned with the helper, not clang's JSON dump (c.md "The toolchain side", "The
//! declaration dump").
//!
//! Tokens are split by one space; `-` is none; a name is a C identifier; a path or a string is
//! lowercase hex of its bytes; every number is decimal; a kind, a cause, a convention, a
//! qualifier set or an attribute set is the model's byte (`IntKind::byte`, `Quals.0`, ...).
//! Ids are positional: the n-th `header` line is `HeaderId(n)`, the n-th `ty` line `TypeId(n)`,
//! and so for `ns`, `record`, `enum`, `typedef` and `var`; a line names only ids made before it.
//! `H AT LEN` is an [`Origin`]: a header id and the declaration's byte range in it.
//!
//! ```text
//! cfront-decls 2
//! abi PTR_SIZE PTR_ALIGN LITTLE CHAR_SIGNED
//! header GUEST_PATH
//! ns NAME PARENT|-
//! ty void Q
//! ty bool SIZE ALIGN Q
//! ty int KIND SIZE ALIGN SIGNED Q
//! ty float KIND SIZE ALIGN Q
//! ty ptr T Q
//! ty ref T CATEGORY
//! ty array T LEN|- Q
//! ty fn RESULT VARIADIC PROTOTYPED CONV PARAM..
//! ty record R Q | ty enum E Q | ty typedef D Q
//! ty valist LOWERED Q
//! ty unexpressible CAUSE SIZE ALIGN
//! record KIND NAME|- NS|- H AT LEN
//! define-record R SIZE ALIGN PACK|- RELOCATION
//!   unit OFFSET SIZE
//!   field NAME|- T OFFSET_BITS WIDTH|- UNIT|- AT LEN
//!   base R OFFSET VIRTUAL
//! end
//! enum NAME|- NS|- H AT LEN
//! define-enum E UNDERLYING COMPLETE SCOPED
//!   const NAME VALUE AT LEN
//! end
//! typedef NAME NS|- T H AT LEN
//! fn NAME NS|- SYMBOL T ATTRS OWNER|- METHOD LINKAGE BODY H AT LEN PARAM_NAME|-..
//! var NAME NS|- SYMBOL T ATTRS LINKAGE INIT H AT LEN        INIT: absent|zero|other|BITS
//! macro int NAME T BITS H AT LEN
//! macro float NAME T F64_BITS H AT LEN
//! macro str NAME T HEX|- H AT LEN
//! macro slot NAME T TABLE INDEX DEREF H AT LEN              TABLE: a variable id
//! macro addr NAME VAR H AT LEN
//! unexpressed NAME NS|- DECL_KIND CAUSE H AT LEN
//! end
//! ```
//!
//! A `define-record` takes its `unit` lines first, then its `field`s, then its `base`s; a
//! field's `UNIT` is the index among its record's units. The reader checks what the builder
//! does not: each `ty int`'s size and alignment against the target's [`CAbi`] for its kind,
//! and the `abi` line against the builder's.

#![blaze::sans_io]

use blaze_c::{
    Bitfield, CAbi, CType, Conv, DeclKind, DeclsBuilder, EnumConst, EnumId, Field, FloatKind, FnAttrs, Function, HeaderId,
    IntKind, Linkage, MacroValue, MethodKind, ModelError, Name, NsId, Origin, Quals, RecordBody, RecordId, RecordKind,
    Relocation, Root, Scalar, TypeId, TypedefId, Unexpressed, ValueCategory, VarAttrs, VarId, VarInit, Variable,
};
use blaze_own::{AVec, Alloc, Vec, planted::Planted};
use blaze_roots::{Mounts, RootId};

use crate::plant::DeclsPlant;

/// The first line.
pub const MAGIC: &str = "cfront-decls 2";

/// What a reader holds itself to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclsLimits {
    pub most_line: u32,
    /// Units, fields and bases of one record, constants of one enum, parameters of one function.
    pub most_members: u32,
    /// Bytes of a header's guest path.
    pub most_path: u32,
}

impl DeclsLimits {
    /// For one import: a line is one declaration, and a function of 64 Ki parameters is not C
    /// any compiler takes (an estimate; CPython's `Python.h` has no line over 1 KiB).
    pub const STANDARD: DeclsLimits = DeclsLimits { most_line: 1 << 20, most_members: 1 << 16, most_path: 4096 };
}

/// What [`DeclsReader::new`] takes: every choice required.
pub struct DeclsReaderOpts<'r, 'a, P> {
    /// The model the lines are applied to, made for the target (its [`blaze_c::AbiRow`]).
    pub into: DeclsBuilder<'a>,
    /// The target's facts, which every integer row is checked against.
    pub abi: CAbi,
    /// How a header's guest path is a root and a path: the run's mounts, and which of
    /// blaze-c's roots each of its roots is.
    pub mounts: &'r Mounts<'r>,
    pub places: &'r [(RootId, Root)],
    pub limits: DeclsLimits,
    pub alloc: Alloc<'a>,
    /// `NoPlant` in shipped code.
    pub plant: P,
}

/// Why a document was refused, and its line (counted from 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclsError {
    pub line: u32,
    pub why: DeclsWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclsWhy {
    /// The first line is not [`MAGIC`] (a helper of another version).
    Magic,
    /// The second is not an `abi` line equal to the builder's.
    Abi,
    NotUtf8,
    LineOver { most: u32 },
    /// A line of no known kind, or one in the wrong place (a `field` outside a record's body).
    UnknownLine,
    /// A token that is not what its place takes; the place named.
    Token { what: &'static str },
    /// Fewer or more tokens than the line's kind takes.
    Arity,
    /// A byte no value of the model's enum has; the enum named.
    Byte { what: &'static str },
    /// An integer row whose size, alignment or sign is not the target's for its kind.
    IntFacts { kind: IntKind },
    /// A header's guest path under no mount, or under a root no blaze-c root is.
    HeaderPlace,
    /// The builder refused the call (a dangling id, a bitfield outside its unit, ..).
    Model(ModelError),
    /// A record or enum body not closed by `end` before the next declaration.
    Unclosed,
    /// The document has no final `end`.
    NotEnded,
    /// Bytes after the final `end`.
    AfterEnd,
    TooMany { what: &'static str },
}

/// What a feed came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    /// More is wanted.
    More,
    /// The final `end` was read.
    Ended,
}

/// What was read, for the ladder's printed counts (c.md "Proof on real libraries").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Census {
    pub headers: u32,
    pub types: u32,
    pub records: u32,
    /// Records given a layout.
    pub layouts: u32,
    pub enums: u32,
    pub typedefs: u32,
    pub functions: u32,
    pub variables: u32,
    pub constants: u32,
    /// Left out, by [`Unexpressed`] byte.
    pub unexpressed: [u32; 64],
}

impl Census {
    const ZERO: Census = Census {
        headers: 0,
        types: 0,
        records: 0,
        layouts: 0,
        enums: 0,
        typedefs: 0,
        functions: 0,
        variables: 0,
        constants: 0,
        unexpressed: [0; 64],
    };

    pub fn left_out(&self) -> u32 {
        self.unexpressed.iter().sum()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Read {
    Magic,
    Abi,
    Body,
    Record { record: RecordId, size: u64, align: u32, pack: Option<u32>, relocation: Relocation, part: Part },
    Enum { decl: EnumId, underlying: TypeId, complete: bool, scoped: bool },
    Ended,
}

/// Where a record body is: its parts come in this order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Part {
    Units,
    Fields,
    Bases,
}

/// The reader, fed in pieces.
pub struct DeclsReader<'r, 'a, P> {
    into: DeclsBuilder<'a>,
    abi: CAbi,
    mounts: &'r Mounts<'r>,
    places: &'r [(RootId, Root)],
    limits: DeclsLimits,
    alloc: Alloc<'a>,
    plant: P,
    read: Read,
    line: u32,
    partial: AVec<'a, u8>,
    census: Census,
    /// The open body's members and a line's lists, reused.
    units: AVec<'a, (u32, u32)>,
    fields: AVec<'a, Field>,
    bases: AVec<'a, (RecordId, u64, bool)>,
    consts: AVec<'a, EnumConst>,
    params: AVec<'a, TypeId>,
    names: AVec<'a, Option<Name>>,
    bytes: AVec<'a, u8>,
    path: AVec<'a, u8>,
}

impl<'r, 'a, P: Planted<DeclsPlant>> DeclsReader<'r, 'a, P> {
    pub fn new(opts: DeclsReaderOpts<'r, 'a, P>) -> DeclsReader<'r, 'a, P> {
        let alloc = opts.alloc;
        DeclsReader {
            into: opts.into,
            abi: opts.abi,
            mounts: opts.mounts,
            places: opts.places,
            limits: opts.limits,
            alloc,
            plant: opts.plant,
            read: Read::Magic,
            line: 0,
            partial: Vec::new_in(alloc),
            census: Census::ZERO,
            units: Vec::new_in(alloc),
            fields: Vec::new_in(alloc),
            bases: Vec::new_in(alloc),
            consts: Vec::new_in(alloc),
            params: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            bytes: Vec::new_in(alloc),
            path: Vec::new_in(alloc),
        }
    }

    /// `bytes` read; a line cut at the end of them is kept for the next feed.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Progress, DeclsError> {
        if self.plant.has(DeclsPlant::LineRestarts) {
            self.partial.clear();
        }
        let mut rest = bytes;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            let (piece, after) = rest.split_at(newline);
            rest = &after[1..];
            if self.partial.is_empty() {
                self.line_done(piece)?;
            } else {
                self.partial.extend_from_slice(piece);
                let whole = core::mem::replace(&mut self.partial, Vec::new_in(self.alloc));
                self.line_done(whole.as_slice())?;
            }
        }
        if !rest.is_empty() {
            if self.read == Read::Ended {
                return Err(self.error(DeclsWhy::AfterEnd));
            }
            self.partial.extend_from_slice(rest);
            if self.partial.len() as u64 > u64::from(self.limits.most_line) {
                return Err(self.error(DeclsWhy::LineOver { most: self.limits.most_line }));
            }
        }
        Ok(if self.read == Read::Ended { Progress::Ended } else { Progress::More })
    }

    /// The model and what was read, once the final `end` was read.
    pub fn finish(self) -> Result<(DeclsBuilder<'a>, Census), DeclsError> {
        if self.read != Read::Ended || !self.partial.is_empty() {
            return Err(self.error(DeclsWhy::NotEnded));
        }
        Ok((self.into, self.census))
    }

    fn error(&self, why: DeclsWhy) -> DeclsError {
        DeclsError { line: self.line, why }
    }

    fn line_done(&mut self, raw: &[u8]) -> Result<(), DeclsError> {
        self.line += 1;
        if self.read == Read::Ended {
            return Err(self.error(DeclsWhy::AfterEnd));
        }
        if raw.len() as u64 > u64::from(self.limits.most_line) {
            return Err(self.error(DeclsWhy::LineOver { most: self.limits.most_line }));
        }
        let text = core::str::from_utf8(raw).map_err(|_| self.error(DeclsWhy::NotUtf8))?;
        self.line_text(text).map_err(|why| self.error(why))
    }

    fn line_text(&mut self, text: &str) -> Result<(), DeclsWhy> {
        match self.read {
            Read::Magic => {
                if text != MAGIC {
                    return Err(DeclsWhy::Magic);
                }
                self.read = Read::Abi;
                Ok(())
            }
            Read::Abi => {
                let mut t = Tokens::new(text);
                let row = self.into.abi();
                let tag = t.word()?;
                let pointer_size = t.num::<u8>("pointer size")?;
                let pointer_align = t.num::<u8>("pointer alignment")?;
                let little_endian = t.num::<u8>("endianness")?;
                let char_signed = t.num::<u8>("char sign")?;
                t.end()?;
                let ok = tag == "abi"
                    && pointer_size == row.pointer_size
                    && pointer_align == row.pointer_align
                    && little_endian == row.little_endian
                    && char_signed == row.char_signed;
                if !ok {
                    return Err(DeclsWhy::Abi);
                }
                self.read = Read::Body;
                Ok(())
            }
            Read::Body => self.body(text),
            Read::Record { .. } => self.record_member(text),
            Read::Enum { .. } => self.enum_member(text),
            Read::Ended => Err(DeclsWhy::AfterEnd),
        }
    }

    fn body(&mut self, text: &str) -> Result<(), DeclsWhy> {
        let mut t = Tokens::new(text);
        let kind = t.word()?;
        match kind {
            "end" => {
                t.end()?;
                self.read = Read::Ended;
            }
            "header" => {
                t.hex_into(&mut self.path, self.limits.most_path)?;
                t.end()?;
                let root = self.place()?;
                let rel = core::str::from_utf8(self.bytes.as_slice()).map_err(|_| DeclsWhy::HeaderPlace)?;
                self.into.header(root, rel).map_err(DeclsWhy::Model)?;
                self.census.headers += 1;
            }
            "ns" => {
                let name = self.name(t.word()?)?;
                let parent = t.ns()?;
                t.end()?;
                self.into.namespace(name, parent).map_err(DeclsWhy::Model)?;
            }
            "ty" => {
                self.ty(&mut t)?;
                self.census.types += 1;
            }
            "record" => {
                let rk = byte(t.num("record kind")?, RecordKind::from_byte, "record kind")?;
                let name = self.opt_name(t.word()?)?;
                let ns = t.ns()?;
                let origin = t.origin()?;
                t.end()?;
                self.into.declare_record(rk, name, ns, origin).map_err(DeclsWhy::Model)?;
                self.census.records += 1;
            }
            "define-record" => {
                let record = RecordId(t.num("record")?);
                let size = t.num("record size")?;
                let align = t.num("record alignment")?;
                let pack = t.opt_num("pack")?;
                let relocation = byte(t.num("relocation")?, Relocation::from_byte, "relocation")?;
                t.end()?;
                self.units.clear();
                self.fields.clear();
                self.bases.clear();
                self.read = Read::Record { record, size, align, pack, relocation, part: Part::Units };
            }
            "enum" => {
                let name = self.opt_name(t.word()?)?;
                let ns = t.ns()?;
                let origin = t.origin()?;
                t.end()?;
                self.into.declare_enum(name, ns, origin).map_err(DeclsWhy::Model)?;
                self.census.enums += 1;
            }
            "define-enum" => {
                let decl = EnumId(t.num("enum")?);
                let underlying = TypeId(t.num("underlying type")?);
                let complete = t.flag("complete")?;
                let scoped = t.flag("scoped")?;
                t.end()?;
                self.consts.clear();
                self.read = Read::Enum { decl, underlying, complete, scoped };
            }
            "typedef" => {
                let name = self.name(t.word()?)?;
                let ns = t.ns()?;
                let ty = TypeId(t.num("type")?);
                let origin = t.origin()?;
                t.end()?;
                self.into.typedef(name, ns, ty, origin).map_err(DeclsWhy::Model)?;
                self.census.typedefs += 1;
            }
            "fn" => {
                let name = self.name(t.word()?)?;
                let ns = t.ns()?;
                let symbol = self.name(t.word()?)?;
                let ty = TypeId(t.num("type")?);
                let attrs = FnAttrs(t.num("function attributes")?);
                let owner = t.opt_num::<u32>("owner")?;
                let method = byte(t.num("method kind")?, MethodKind::from_byte, "method kind")?;
                let linkage = byte(t.num("linkage")?, Linkage::from_byte, "linkage")?;
                let body = t.flag("body")?;
                let origin = t.origin()?;
                self.names.clear();
                while let Some(word) = t.next() {
                    if self.names.len() as u64 >= u64::from(self.limits.most_members) {
                        return Err(DeclsWhy::TooMany { what: "parameter names" });
                    }
                    let name = self.opt_name(word)?;
                    self.names.push(name);
                }
                let owner = match (owner, method) {
                    (None, MethodKind::Free) => None,
                    (Some(record), kind) if kind != MethodKind::Free => Some((RecordId(record), kind)),
                    _ => return Err(DeclsWhy::Token { what: "owner and method kind" }),
                };
                let f = Function { name, ns, symbol, ty, param_names: self.names.as_slice(), attrs, owner, linkage, body, origin };
                self.into.function(f).map_err(DeclsWhy::Model)?;
                self.census.functions += 1;
            }
            "var" => {
                let name = self.name(t.word()?)?;
                let ns = t.ns()?;
                let symbol = self.name(t.word()?)?;
                let ty = TypeId(t.num("type")?);
                let attrs = VarAttrs(t.num("variable attributes")?);
                let linkage = byte(t.num("linkage")?, Linkage::from_byte, "linkage")?;
                let init = match t.word()? {
                    "absent" => VarInit::Absent,
                    "zero" => VarInit::Zero,
                    "other" => VarInit::Other,
                    bits => VarInit::Int(number(bits, "variable initializer")?),
                };
                let origin = t.origin()?;
                t.end()?;
                let made = self.into.variable(Variable { name, ns, symbol, ty, attrs, linkage, init, origin });
                let id = made.map_err(DeclsWhy::Model)?;
                // Ids are positional: the builder's must be the line's count.
                if id != VarId(self.census.variables) {
                    return Err(DeclsWhy::Token { what: "variable id" });
                }
                self.census.variables += 1;
            }
            "macro" => {
                let what = t.word()?;
                let name = self.name(t.word()?)?;
                if what == "addr" {
                    let variable = VarId(t.num("variable")?);
                    let origin = t.origin()?;
                    t.end()?;
                    self.into.constant(name, MacroValue::AddressOf { variable }, origin).map_err(DeclsWhy::Model)?;
                    self.census.constants += 1;
                    return Ok(());
                }
                let ty = TypeId(t.num("macro type")?);
                match what {
                    "slot" => {
                        let table = VarId(t.num("table")?);
                        let index = t.num("slot index")?;
                        let deref = t.flag("deref")?;
                        let origin = t.origin()?;
                        t.end()?;
                        let value = MacroValue::Slot { table, index, ty, deref };
                        self.into.constant(name, value, origin).map_err(DeclsWhy::Model)?;
                    }
                    "int" => {
                        let bits = t.num("macro value")?;
                        let origin = t.origin()?;
                        t.end()?;
                        self.into.constant(name, MacroValue::Int { bits, ty }, origin).map_err(DeclsWhy::Model)?;
                    }
                    "float" => {
                        let bits: u64 = t.num("macro value")?;
                        let origin = t.origin()?;
                        t.end()?;
                        let value = f64::from_bits(bits);
                        self.into.constant(name, MacroValue::Float { value, ty }, origin).map_err(DeclsWhy::Model)?;
                    }
                    "str" => {
                        let most = self.limits.most_line;
                        let mut bytes = core::mem::replace(&mut self.bytes, Vec::new_in(self.alloc));
                        let got = t.opt_hex_into(&mut bytes, most);
                        let origin = got.and_then(|()| t.origin());
                        let ended = origin.and_then(|origin| t.end().map(|()| origin));
                        let made = ended.and_then(|origin| {
                            let value = MacroValue::Str { bytes: bytes.as_slice(), ty };
                            self.into.constant(name, value, origin).map_err(DeclsWhy::Model)
                        });
                        self.bytes = bytes;
                        made?;
                    }
                    _ => return Err(DeclsWhy::Token { what: "macro value kind" }),
                }
                self.census.constants += 1;
            }
            "unexpressed" => {
                let name = self.name(t.word()?)?;
                let ns = t.ns()?;
                let kind = byte(t.num("declaration kind")?, DeclKind::from_byte, "declaration kind")?;
                let cause = byte(t.num("cause")?, Unexpressed::from_byte, "cause")?;
                let origin = t.origin()?;
                t.end()?;
                self.into.unexpressed(name, ns, kind, cause, origin).map_err(DeclsWhy::Model)?;
                if let Some(n) = self.census.unexpressed.get_mut(usize::from(cause.byte())) {
                    *n += 1;
                }
            }
            _ => return Err(DeclsWhy::UnknownLine),
        }
        Ok(())
    }

    /// The `ty` line in `t` applied.
    fn ty(&mut self, t: &mut Tokens<'_>) -> Result<(), DeclsWhy> {
        let shape = t.word()?;
        let ty = match shape {
            "void" => CType::Void { quals: t.quals()? },
            "bool" => CType::Bool { size: t.num("size")?, align: t.num("alignment")?, quals: t.quals()? },
            "int" => {
                let kind = byte(t.num("integer kind")?, IntKind::from_byte, "integer kind")?;
                let size = t.num("size")?;
                let align = t.num("alignment")?;
                let signed = t.flag("signed")?;
                if !self.int_facts(kind, size, align, signed) {
                    return Err(DeclsWhy::IntFacts { kind });
                }
                CType::Int { kind, size, align, signed, quals: t.quals()? }
            }
            "float" => {
                let kind = byte(t.num("float kind")?, FloatKind::from_byte, "float kind")?;
                CType::Float { kind, size: t.num("size")?, align: t.num("alignment")?, quals: t.quals()? }
            }
            "ptr" => CType::Pointer { pointee: TypeId(t.num("pointee")?), quals: t.quals()? },
            "ref" => CType::Reference {
                pointee: TypeId(t.num("pointee")?),
                category: byte(t.num("value category")?, ValueCategory::from_byte, "value category")?,
            },
            "array" => CType::Array { elem: TypeId(t.num("element")?), len: t.opt_num("length")?, quals: t.quals()? },
            "fn" => {
                let result = TypeId(t.num("result")?);
                let variadic = t.flag("variadic")?;
                let prototyped = t.flag("prototyped")?;
                let conv = byte(t.num("convention")?, Conv::from_byte, "convention")?;
                self.params.clear();
                while let Some(word) = t.next() {
                    if self.params.len() as u64 >= u64::from(self.limits.most_members) {
                        return Err(DeclsWhy::TooMany { what: "parameters" });
                    }
                    self.params.push(TypeId(number(word, "parameter")?));
                }
                let made = self.into.ty(CType::Function { result, params: self.params.as_slice(), variadic, prototyped, conv });
                return made.map(|_| ()).map_err(DeclsWhy::Model);
            }
            "record" => CType::Record { record: RecordId(t.num("record")?), quals: t.quals()? },
            "enum" => CType::Enum { decl: EnumId(t.num("enum")?), quals: t.quals()? },
            "typedef" => CType::Typedef { decl: TypedefId(t.num("typedef")?), quals: t.quals()? },
            "valist" => CType::VaList { lowered: TypeId(t.num("lowered type")?), quals: t.quals()? },
            "unexpressible" => CType::Unexpressible {
                cause: byte(t.num("cause")?, Unexpressed::from_byte, "cause")?,
                size: t.num("size")?,
                align: t.num("alignment")?,
            },
            _ => return Err(DeclsWhy::Token { what: "type shape" }),
        };
        t.end()?;
        self.into.ty(ty).map(|_| ()).map_err(DeclsWhy::Model)
    }

    fn record_member(&mut self, text: &str) -> Result<(), DeclsWhy> {
        let Read::Record { record, size, align, pack, relocation, part } = self.read else {
            return Err(DeclsWhy::UnknownLine);
        };
        let mut t = Tokens::new(text);
        let kind = t.word()?;
        let most = self.limits.most_members as usize;
        let at = match kind {
            "unit" => Part::Units,
            "field" => Part::Fields,
            "base" => Part::Bases,
            "end" => {
                t.end()?;
                let body = RecordBody {
                    size,
                    align,
                    pack,
                    fields: self.fields.as_slice(),
                    units: self.units.as_slice(),
                    bases: self.bases.as_slice(),
                    relocation,
                };
                self.into.define_record(record, body).map_err(DeclsWhy::Model)?;
                self.census.layouts += 1;
                self.read = Read::Body;
                return Ok(());
            }
            _ => return Err(DeclsWhy::Unclosed),
        };
        if at < part {
            return Err(DeclsWhy::UnknownLine);
        }
        if self.units.len() + self.fields.len() + self.bases.len() >= most {
            return Err(DeclsWhy::TooMany { what: "record members" });
        }
        match at {
            Part::Units => {
                let unit = (t.num("unit offset")?, t.num("unit size")?);
                t.end()?;
                self.units.push(unit);
            }
            Part::Fields => {
                let name = self.opt_name(t.word()?)?;
                let ty = TypeId(t.num("field type")?);
                let offset_bits = t.num("field offset")?;
                let width = t.opt_num::<u32>("bitfield width")?;
                let unit = t.opt_num::<u32>("bitfield unit")?;
                let (at_, len) = (t.num("field start")?, t.num("field length")?);
                t.end()?;
                let bitfield = match (width, unit) {
                    (None, None) => None,
                    (Some(width), Some(unit)) => Some(Bitfield { width, unit }),
                    _ => return Err(DeclsWhy::Token { what: "bitfield width and unit" }),
                };
                self.fields.push(Field { name, ty, offset_bits, bitfield, at: at_, len });
            }
            Part::Bases => {
                let base = (RecordId(t.num("base")?), t.num("base offset")?, t.flag("virtual")?);
                t.end()?;
                self.bases.push(base);
            }
        }
        self.read = Read::Record { record, size, align, pack, relocation, part: at };
        Ok(())
    }

    fn enum_member(&mut self, text: &str) -> Result<(), DeclsWhy> {
        let Read::Enum { decl, underlying, complete, scoped } = self.read else {
            return Err(DeclsWhy::UnknownLine);
        };
        let mut t = Tokens::new(text);
        match t.word()? {
            "const" => {
                if self.consts.len() as u64 >= u64::from(self.limits.most_members) {
                    return Err(DeclsWhy::TooMany { what: "enum constants" });
                }
                let name = self.name(t.word()?)?;
                let value = t.num("enum value")?;
                let (at, len) = (t.num("constant start")?, t.num("constant length")?);
                t.end()?;
                self.consts.push(EnumConst { name, value, at, len });
                Ok(())
            }
            "end" => {
                t.end()?;
                self.into.define_enum(decl, underlying, self.consts.as_slice(), complete, scoped).map_err(DeclsWhy::Model)?;
                self.read = Read::Body;
                Ok(())
            }
            _ => Err(DeclsWhy::Unclosed),
        }
    }

    /// The header path in `self.path` as a blaze-c root, its path under it left in `self.bytes`.
    fn place(&mut self) -> Result<Root, DeclsWhy> {
        let root = self
            .mounts
            .resolve(self.path.as_slice(), self.limits.most_path, &mut self.bytes)
            .map_err(|_| DeclsWhy::HeaderPlace)?;
        self.places.iter().find(|(id, _)| *id == root).map(|(_, place)| *place).ok_or(DeclsWhy::HeaderPlace)
    }

    fn name(&mut self, word: &str) -> Result<Name, DeclsWhy> {
        self.into.name(word).map_err(DeclsWhy::Model)
    }

    fn opt_name(&mut self, word: &str) -> Result<Option<Name>, DeclsWhy> {
        match word {
            "-" => Ok(None),
            word => self.name(word).map(Some),
        }
    }

    /// Whether an integer row agrees with the target's facts for its kind.
    fn int_facts(&self, kind: IntKind, size: u32, align: u32, signed: bool) -> bool {
        let abi = &self.abi;
        let one = Scalar { size: 1, align: 1 };
        let (scalar, want_signed) = match kind {
            IntKind::Char => (Some(one), abi.char_signed),
            IntKind::SChar => (Some(one), true),
            IntKind::UChar => (Some(one), false),
            IntKind::Short => (Some(abi.short), true),
            IntKind::UShort => (Some(abi.short), false),
            IntKind::Int => (Some(abi.int), true),
            IntKind::UInt => (Some(abi.int), false),
            IntKind::Long => (Some(abi.long), true),
            IntKind::ULong => (Some(abi.long), false),
            IntKind::LongLong => (Some(abi.long_long), true),
            IntKind::ULongLong => (Some(abi.long_long), false),
            IntKind::Int128 => (abi.int128, true),
            IntKind::UInt128 => (abi.int128, false),
        };
        scalar.is_some_and(|s| u32::from(s.size) == size && u32::from(s.align) == align) && signed == want_signed
    }
}

fn byte<T>(value: u8, from: fn(u8) -> Option<T>, what: &'static str) -> Result<T, DeclsWhy> {
    from(value).ok_or(DeclsWhy::Byte { what })
}

fn number<T: core::str::FromStr>(s: &str, what: &'static str) -> Result<T, DeclsWhy> {
    // One spelling of each number: no sign, no leading zero (rule 25: equal models, equal text).
    let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if !canonical {
        return Err(DeclsWhy::Token { what });
    }
    s.parse().map_err(|_| DeclsWhy::Token { what })
}

/// A line's tokens.
struct Tokens<'t> {
    words: core::str::Split<'t, char>,
}

impl<'t> Tokens<'t> {
    fn new(text: &'t str) -> Tokens<'t> {
        Tokens { words: text.split(' ') }
    }

    fn next(&mut self) -> Option<&'t str> {
        self.words.next()
    }

    fn word(&mut self) -> Result<&'t str, DeclsWhy> {
        match self.words.next() {
            Some("") | None => Err(DeclsWhy::Arity),
            Some(word) => Ok(word),
        }
    }

    fn end(&mut self) -> Result<(), DeclsWhy> {
        match self.words.next() {
            None => Ok(()),
            Some(_) => Err(DeclsWhy::Arity),
        }
    }

    fn num<T: core::str::FromStr>(&mut self, what: &'static str) -> Result<T, DeclsWhy> {
        number(self.word()?, what)
    }

    fn opt_num<T: core::str::FromStr>(&mut self, what: &'static str) -> Result<Option<T>, DeclsWhy> {
        match self.word()? {
            "-" => Ok(None),
            word => number(word, what).map(Some),
        }
    }

    fn flag(&mut self, what: &'static str) -> Result<bool, DeclsWhy> {
        match self.word()? {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(DeclsWhy::Token { what }),
        }
    }

    fn quals(&mut self) -> Result<Quals, DeclsWhy> {
        let q: u8 = self.num("qualifiers")?;
        if q & !7 != 0 {
            return Err(DeclsWhy::Byte { what: "qualifiers" });
        }
        Ok(Quals(q))
    }

    fn ns(&mut self) -> Result<NsId, DeclsWhy> {
        Ok(self.opt_num("namespace")?.map_or(NsId::GLOBAL, NsId))
    }

    fn origin(&mut self) -> Result<Origin, DeclsWhy> {
        Ok(Origin { header: HeaderId(self.num("header")?), at: self.num("start")?, len: self.num("length")? })
    }

    /// Lowercase hex of at most `most` bytes, into `out` (cleared first).
    fn hex_into(&mut self, out: &mut AVec<'_, u8>, most: u32) -> Result<(), DeclsWhy> {
        let word = self.word()?;
        hex(word, out, most)
    }

    fn opt_hex_into(&mut self, out: &mut AVec<'_, u8>, most: u32) -> Result<(), DeclsWhy> {
        match self.word()? {
            "-" => {
                out.clear();
                Ok(())
            }
            word => hex(word, out, most),
        }
    }
}

fn hex(word: &str, out: &mut AVec<'_, u8>, most: u32) -> Result<(), DeclsWhy> {
    out.clear();
    let digits = word.as_bytes();
    if digits.len() % 2 != 0 || digits.len() / 2 > most as usize {
        return Err(DeclsWhy::Token { what: "hex" });
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(DeclsWhy::Token { what: "hex" }),
    };
    for pair in digits.chunks_exact(2) {
        out.push(nibble(pair[0])? << 4 | nibble(pair[1])?);
    }
    Ok(())
}
