//! [`Builder`]: what the MIR lowering calls to make a LIR function, its surface shaped like
//! rustc's `BuilderMethods` (an instruction per call, the ABI decided before).
//!
//! It writes into a [`Sink`]:
//!
//! - [`Full`] makes the function's columns: [`Builder::finish`] gives a [`Func`].
//! - [`RequestsOnly`] makes no instructions, only what the function refers to (functions,
//!   statics and thread-locals by place, its own data by content, target intrinsics by name),
//!   and the type of each value so the lowering can still ask it: the walk that lists an
//!   instance's references before any unit compiles it is the lowering run on this sink, so
//!   no hand-kept mirror of the lowering is needed.
//!
//! # Blocks
//!
//! A block is made ([`Builder::create_block`]) when something first needs to name it, and
//! filled in one stretch: [`Builder::begin`] it, add its parameters, then its instructions,
//! ending with a terminator, before beginning the next. Blocks are laid out in the order they
//! are begun, so values (instruction indices) are numbered in layout order as they are made.
//! The [`Block`]s the builder hands out are in the order they were made; [`Builder::finish`]
//! rewrites every edge to the layout order, the one renumbering, of blocks only. A block made
//! and never begun is dropped if no edge names it, and an error if one does.
//!
//! # Errors
//!
//! A misuse (an instruction with no block begun, a parameter after an instruction, a block
//! begun twice) does not panic: the builder keeps the first error, goes on, and
//! [`Builder::finish`] returns it, naming the operation and block. In debug builds `finish`
//! also runs the verifier.

use std::{alloc::Allocator, hash::BuildHasher, marker::PhantomData};

use blaze_format::SpanRef;
use blaze_hash::{FixedState, HashMap};

use crate::{
    flags::{AtomicRmw, Flags, FloatCond, IntCond},
    func::{
        AsmId, AsmOperandKind, AsmOperandRec, AsmPieceRec, AsmRec, Block, BlockKind, CompoundRec,
        Conv, DataId, DataRec, DataRelocRec, Ext, FnAttrs, Func, Head, InlinedRec, IntrinsicId,
        IntrinsicRec, MemAttrsRec, NONE, ParamFactsRec, ParamKind, ParamRec, RangeRec, RefId,
        RefKind, RefRec, SigId, SigRec, SlotId, SlotKind, SlotRec, Value, VarRec,
    },
    op::{Layout, Op},
    ty::{Form, Scalar, Ty},
    verify::{self, VerifyError},
};

/// What a [`Builder`] writes.
pub trait Sink {
    const MAKES: Makes;
}

/// Whether a sink makes code or only lists requests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Makes {
    Code,
    Requests,
}

/// The sink that makes the function.
#[derive(Clone, Copy, Debug)]
pub struct Full;

/// The sink that makes no instructions, only the function's requests ([`Requests`]).
#[derive(Clone, Copy, Debug)]
pub struct RequestsOnly;

impl Sink for Full {
    const MAKES: Makes = Makes::Code;
}

impl Sink for RequestsOnly {
    const MAKES: Makes = Makes::Requests;
}

/// What a requests-only build gives: a function with its tables and no instructions. Its
/// `refs`, `datas` and `intrinsics` (with `text` for their names) are the requests, in the
/// order the lowering first made each, which is the order a full build of the same lowering
/// makes them in.
pub struct Requests<A: Allocator>(pub Func<A>);

/// Why a [`Builder`] made no function.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// An instruction with no block begun.
    NoBlock { op: Op },
    /// An instruction after its block's terminator.
    AfterEnd { op: Op, block: Block },
    /// A parameter after an instruction of its block.
    LateParam { block: Block },
    /// A block begun, or the function finished, while this one has no terminator.
    Unterminated { block: Block },
    /// A block begun a second time.
    Begun { block: Block },
    /// A block an edge names that was never begun.
    Unbegun { block: Block },
    /// A block that was never made.
    NoSuchBlock { block: Block },
    /// No signature was set ([`Builder::set_signature`]).
    NoSignature,
    /// An operand the builder cannot make the operation of: `rule` says which.
    Operand { op: Op, rule: &'static str },
    /// A table past what its index can name: `table` names it.
    Full { table: &'static str },
    /// The function made breaks a rule of the verifier: a lowering bug.
    Verify(VerifyError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::NoBlock { op } => write!(f, "a `{}` made with no block begun", op.name()),
            BuildError::AfterEnd { op, block } => {
                write!(f, "a `{}` made in {block} after its terminator", op.name())
            }
            BuildError::LateParam { block } => {
                write!(f, "a parameter of {block} made after one of its instructions")
            }
            BuildError::Unterminated { block } => write!(f, "{block} has no terminator"),
            BuildError::Begun { block } => write!(f, "{block} begun a second time"),
            BuildError::Unbegun { block } => {
                write!(f, "{block} (in the order blocks were made) is named by an edge and never begun")
            }
            BuildError::NoSuchBlock { block } => write!(f, "{block} was never made"),
            BuildError::NoSignature => f.write_str("a function with no signature set"),
            BuildError::Operand { op, rule } => write!(f, "a `{}` of {rule}", op.name()),
            BuildError::Full { table } => write!(f, "more {table} than LIR can index"),
            BuildError::Verify(error) => write!(f, "the function made is not valid LIR: {error}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Whether a signature is C's `...`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Variadic {
    No,
    Yes,
}

/// A valid range, `start..=end` wrapping as rustc's `WrappingRange`.
pub type Range = (u128, u128);

/// What a parameter or return is told: [`ParamFactsRec`] with its range by value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Facts {
    pub deref: u64,
    pub size: u32,
    pub range: Option<Range>,
    pub align: u8,
    /// [`ParamFactsRec`]'s bits.
    pub bits: u8,
}

/// A parameter or return of a signature.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Param {
    pub ty: Ty,
    pub ext: Ext,
    pub kind: ParamKind,
    pub facts: Facts,
}

impl Param {
    /// A plain parameter of `ty`, told nothing.
    pub const fn of(ty: Ty) -> Param {
        Param {
            ty,
            ext: Ext::None,
            kind: ParamKind::Value,
            facts: Facts { deref: 0, size: 0, range: None, align: 0, bits: 0 },
        }
    }
}

/// What a memory operation is told: [`MemAttrsRec`] with its range by value. The default
/// claims nothing (alignment 1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MemAttrs {
    pub align: u8,
    pub src_align: u8,
    pub pointee_align: u8,
    /// [`MemAttrsRec`]'s bits.
    pub bits: u8,
    pub range: Option<Range>,
}

impl MemAttrs {
    /// An access aligned to `1 << align`, told nothing else.
    pub const fn aligned(align: u8) -> MemAttrs {
        MemAttrs { align, src_align: 0, pointee_align: 0, bits: 0, range: None }
    }
}

/// An address in data.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DataReloc {
    pub offset: u32,
    pub target: RefId,
    pub addend: i64,
}

/// A piece of an `asm!` template.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AsmPiece<'a> {
    Text(&'a str),
    /// Operand `index` of the block, with its modifier (`{0:e}`).
    Operand { index: u32, modifier: Option<char> },
}

/// Whether an `asm!` operand names a register or a class of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RegKind {
    Reg,
    Class,
}

/// An `asm!` operand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AsmOperand<'a> {
    pub kind: AsmOperandKind,
    /// The register or class; empty for `sym` and `label`.
    pub reg: &'a str,
    pub reg_kind: RegKind,
    /// A `sym`'s row of `refs`, a `label`'s place among the `asm_br`'s labels, else 0.
    pub aux: u32,
}

/// A block and the arguments its parameters take.
pub type To<'a> = (Block, &'a [Value]);

/// Makes one LIR function into the sink `S`, its columns and tables in `A` (the compile's
/// frame).
pub struct Builder<A: Allocator + Clone, S: Sink> {
    func: Func<A>,
    sink: PhantomData<S>,
    /// Per block made: its place in layout order, [`NONE`] until begun.
    places: Vec<u32, A>,
    /// Per block made: its kind.
    kinds: Vec<u8, A>,
    /// Per block made: 1 once an edge names it.
    named: Vec<u8, A>,
    /// The block being filled, as made; [`NONE`] for none.
    current: u32,
    /// How many blocks have been begun.
    begun: u32,
    /// Whether the current block has only parameters so far.
    params_open: bool,
    /// Whether the current block has its terminator.
    ended: bool,
    /// Where in `extra` a block is named, rewritten to layout order by `finish`.
    fixups: Vec<u32, A>,
    /// The `locs` word of the instructions being made.
    loc: u32,
    signature: Option<SigId>,
    error: Option<BuildError>,
    compound_rows: HashMap<u64, u32, A>,
    sig_rows: HashMap<u64, u32, A>,
    fact_rows: HashMap<ParamFactsRec, u32, A>,
    attr_rows: HashMap<MemAttrsRec, u32, A>,
    range_rows: HashMap<RangeRec, u32, A>,
    ref_rows: HashMap<RefRec, u32, A>,
    data_rows: HashMap<u64, u32, A>,
    intrinsic_rows: HashMap<u64, u32, A>,
    span_rows: HashMap<SpanRef, u32, A>,
}

/// The row of `hash` in a table of `rows` rows: `Ok` with the one `same` holds of, or `Err`
/// with the row a new one takes. A hash shared by two rows finds the second by a scan.
fn intern<A: Allocator + Clone>(
    map: &mut HashMap<u64, u32, A>,
    hash: u64,
    rows: u32,
    same: impl Fn(u32) -> bool,
) -> Result<u32, u32> {
    match map.get(&hash) {
        Some(&row) if same(row) => Ok(row),
        Some(_) => (0..rows).find(|&row| same(row)).ok_or(rows),
        None => {
            map.insert(hash, rows);
            Err(rows)
        }
    }
}

fn row(len: usize, table: &'static str) -> Result<u32, BuildError> {
    u32::try_from(len).map_err(|_| BuildError::Full { table })
}

/// The `words` little-endian words of `value`.
fn words_of(value: u128, words: u32) -> impl Iterator<Item = u32> {
    (0..words).map(move |i| (value >> (32 * i)) as u32)
}

impl<A: Allocator + Clone, S: Sink> Builder<A, S> {
    /// A builder for a function of a target whose pointers are `ptr_bits` wide.
    pub fn new_in(ptr_bits: u32, alloc: A) -> Builder<A, S> {
        let head = Head { ptr_bits, sig: 0, attrs: 0 };
        Builder {
            func: Func::new_in(head, alloc.clone()),
            sink: PhantomData,
            places: Vec::new_in(alloc.clone()),
            kinds: Vec::new_in(alloc.clone()),
            named: Vec::new_in(alloc.clone()),
            current: NONE,
            begun: 0,
            params_open: false,
            ended: false,
            fixups: Vec::new_in(alloc.clone()),
            loc: 0,
            signature: None,
            error: None,
            compound_rows: HashMap::new_in(alloc.clone()),
            sig_rows: HashMap::new_in(alloc.clone()),
            fact_rows: HashMap::new_in(alloc.clone()),
            attr_rows: HashMap::new_in(alloc.clone()),
            range_rows: HashMap::new_in(alloc.clone()),
            ref_rows: HashMap::new_in(alloc.clone()),
            data_rows: HashMap::new_in(alloc.clone()),
            intrinsic_rows: HashMap::new_in(alloc.clone()),
            span_rows: HashMap::new_in(alloc),
        }
    }

    fn fail(&mut self, error: BuildError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// The first error the builder met, if it met one.
    pub fn error(&self) -> Option<&BuildError> {
        self.error.as_ref()
    }

    /// The function as made so far.
    pub fn func(&self) -> &Func<A> {
        &self.func
    }

    pub fn ptr_bits(&self) -> u32 {
        self.func.head.ptr_bits
    }

    /// The integer of the pointer's width: `ptr_addr`'s result.
    pub fn usize_ty(&self) -> Ty {
        Ty::int(self.ptr_bits()).unwrap_or(Ty::I64)
    }

    /// The type of `value`.
    pub fn ty(&self, value: Value) -> Ty {
        self.func.tys.get(value.index()).map_or(Ty::NONE, |&it| Ty(it))
    }

    /// The members of compound type `ty`.
    pub fn members(&self, ty: Ty) -> &[u16] {
        let Some(row) = ty.compound_row() else { return &[] };
        let Some(rec) = self.func.compounds.get(row as usize) else { return &[] };
        &self.func.compound_tys[rec.first as usize..][..rec.len as usize]
    }

    // Tables.

    /// The compound type of `members` (each a scalar or a vector).
    pub fn compound(&mut self, members: &[Ty]) -> Ty {
        let hash = FixedState.hash_one(members);
        let func = &self.func;
        let same = |row: u32| {
            let rec = func.compounds[row as usize];
            let have = &func.compound_tys[rec.first as usize..][..rec.len as usize];
            have.len() == members.len() && have.iter().zip(members).all(|(&a, b)| a == b.0)
        };
        let rows = self.func.compounds.len() as u32;
        match intern(&mut self.compound_rows, hash, rows, same) {
            Ok(row) => Ty::compound(row).unwrap_or(Ty::NONE),
            Err(row) => {
                let Some(ty) = Ty::compound(row) else {
                    self.fail(BuildError::Full { table: "compound types" });
                    return Ty::NONE;
                };
                let first = self.func.compound_tys.len() as u32;
                self.func.compound_tys.extend(members.iter().map(|it| it.0));
                self.func.compounds.push(CompoundRec { first, len: members.len() as u32 });
                ty
            }
        }
    }

    /// The range field (1 + its row) of `range`, 0 for none.
    pub fn range(&mut self, range: Option<Range>) -> u32 {
        let Some((start, end)) = range else { return 0 };
        let rec = RangeRec { start, end };
        if let Some(&row) = self.range_rows.get(&rec) {
            return row + 1;
        }
        let row = self.func.ranges.len() as u32;
        self.func.ranges.push(rec);
        self.range_rows.insert(rec, row);
        row + 1
    }

    fn facts(&mut self, facts: Facts) -> u32 {
        let range = self.range(facts.range);
        let rec = ParamFactsRec {
            deref: facts.deref,
            size: facts.size,
            range,
            align: facts.align,
            bits: facts.bits,
            pad: [0; 6],
        };
        if rec == ParamFactsRec::default() {
            return 0;
        }
        if let Some(&row) = self.fact_rows.get(&rec) {
            return row + 1;
        }
        let row = self.func.param_facts.len() as u32;
        self.func.param_facts.push(rec);
        self.fact_rows.insert(rec, row);
        row + 1
    }

    /// The `attrs` word (1 + its row) of `attrs`, 0 for attributes that claim nothing.
    pub fn mem(&mut self, attrs: MemAttrs) -> u32 {
        let range = self.range(attrs.range);
        let rec = MemAttrsRec {
            range,
            align: attrs.align,
            src_align: attrs.src_align,
            pointee_align: attrs.pointee_align,
            bits: attrs.bits,
        };
        if rec == MemAttrsRec::default() {
            return 0;
        }
        if let Some(&row) = self.attr_rows.get(&rec) {
            return row + 1;
        }
        let row = self.func.mem_attrs.len() as u32;
        self.func.mem_attrs.push(rec);
        self.attr_rows.insert(rec, row);
        row + 1
    }

    /// A signature, made once however often it is asked for.
    pub fn sig(&mut self, conv: Conv, variadic: Variadic, params: &[Param], rets: &[Param]) -> SigId {
        let bits = match variadic {
            Variadic::No => 0,
            Variadic::Yes => SigRec::VARIADIC,
        };
        let alloc = self.func.sig_params.allocator().clone();
        let mut recs = Vec::with_capacity_in(params.len() + rets.len(), alloc);
        for param in params.iter().chain(rets) {
            let facts = self.facts(param.facts);
            recs.push(ParamRec { facts, ty: param.ty.0, ext: param.ext as u8, kind: param.kind as u8 });
        }
        let hash = FixedState.hash_one((conv as u8, bits, params.len(), rets.len(), &recs[..]));
        let func = &self.func;
        let same = |row: u32| {
            let have = func.sigs[row as usize];
            let len = have.params as usize + have.rets as usize;
            have.conv == conv as u8
                && have.bits == bits
                && have.params as usize == params.len()
                && have.rets as usize == rets.len()
                && func.sig_params[have.first as usize..][..len] == recs[..]
        };
        let rows = self.func.sigs.len() as u32;
        match intern(&mut self.sig_rows, hash, rows, same) {
            Ok(row) => SigId(row),
            Err(row) => {
                let (Ok(n_params), Ok(n_rets)) = (u16::try_from(params.len()), u16::try_from(rets.len()))
                else {
                    self.fail(BuildError::Full { table: "parameters of a signature" });
                    return SigId(0);
                };
                let first = self.func.sig_params.len() as u32;
                self.func.sig_params.extend_from_slice(&recs);
                self.func.sigs.push(SigRec {
                    first,
                    params: n_params,
                    rets: n_rets,
                    conv: conv as u8,
                    bits,
                    pad: [0; 2],
                });
                SigId(row)
            }
        }
    }

    /// Sets the function's own signature. Its entry block's parameters are its parameters.
    pub fn set_signature(&mut self, sig: SigId) {
        self.signature = Some(sig);
        self.func.head.sig = sig.0;
    }

    pub fn set_attrs(&mut self, attrs: FnAttrs) {
        self.func.head.attrs = attrs.0;
    }

    /// Sets the function's target features, LLVM's `"target-features"` (`+avx2,+fma`).
    pub fn set_features(&mut self, features: &str) {
        self.func.features.clear();
        self.func.features.extend_from_slice(features.as_bytes());
    }

    /// A function, static or thread-local by its place in the instance's references.
    pub fn reference(&mut self, kind: RefKind, place: u32) -> RefId {
        let rec = RefRec { place, kind: kind as u32 };
        if let Some(&row) = self.ref_rows.get(&rec) {
            return RefId(row);
        }
        let row = self.func.refs.len() as u32;
        self.func.refs.push(rec);
        self.ref_rows.insert(rec, row);
        RefId(row)
    }

    /// The function's own data, made once per content, aligned to `1 << align`.
    pub fn data(&mut self, bytes: &[u8], align: u32, relocs: &[DataReloc]) -> RefId {
        let hash = FixedState.hash_one((bytes, align, relocs));
        let func = &self.func;
        let same = |row: u32| {
            let rec = func.datas[row as usize];
            let have_bytes = &func.data_bytes[rec.bytes_first as usize..][..rec.bytes_len as usize];
            let have_relocs =
                &func.data_relocs[rec.relocs_first as usize..][..rec.relocs_len as usize];
            rec.align == align
                && have_bytes == bytes
                && have_relocs.len() == relocs.len()
                && have_relocs.iter().zip(relocs).all(|(a, b)| {
                    a.offset == b.offset && a.target == b.target.0 && a.addend == b.addend
                })
        };
        let rows = self.func.datas.len() as u32;
        let row = match intern(&mut self.data_rows, hash, rows, same) {
            Ok(row) => row,
            Err(row) => {
                let bytes_first = self.func.data_bytes.len() as u32;
                self.func.data_bytes.extend_from_slice(bytes);
                let relocs_first = self.func.data_relocs.len() as u32;
                self.func.data_relocs.extend(relocs.iter().map(|it| DataRelocRec {
                    addend: it.addend,
                    offset: it.offset,
                    target: it.target.0,
                }));
                self.func.datas.push(DataRec {
                    bytes_first,
                    bytes_len: bytes.len() as u32,
                    relocs_first,
                    relocs_len: relocs.len() as u32,
                    align,
                });
                row
            }
        };
        self.reference(RefKind::Data, row)
    }

    /// The data row a [`RefKind::Data`] reference names.
    pub fn data_of(&self, reference: RefId) -> Option<DataId> {
        let rec = self.func.refs.get(reference.index())?;
        (rec.kind == RefKind::Data as u32).then_some(DataId(rec.place))
    }

    /// A target intrinsic (`llvm.*`) of signature `sig`, made once per name and signature.
    pub fn intrinsic(&mut self, name: &str, sig: SigId) -> IntrinsicId {
        let hash = FixedState.hash_one((name, sig.0));
        let func = &self.func;
        let same = |row: u32| {
            let rec = func.intrinsics[row as usize];
            rec.sig == sig.0
                && func.names[rec.name_first as usize..][..rec.name_len as usize] == *name.as_bytes()
        };
        let rows = self.func.intrinsics.len() as u32;
        match intern(&mut self.intrinsic_rows, hash, rows, same) {
            Ok(row) => IntrinsicId(row),
            Err(row) => {
                let name_first = self.func.names.len() as u32;
                self.func.names.extend_from_slice(name.as_bytes());
                self.func.intrinsics.push(IntrinsicRec {
                    name_first,
                    name_len: name.len() as u32,
                    sig: sig.0,
                });
                IntrinsicId(row)
            }
        }
    }

    fn push_text(&mut self, text: &str) -> u32 {
        let first = self.func.asm_text.len() as u32;
        self.func.asm_text.extend_from_slice(text.as_bytes());
        first
    }

    /// A stack slot of `size` bytes aligned to `1 << align`.
    pub fn slot(&mut self, size: u32, align: u8, kind: SlotKind) -> SlotId {
        let row = self.func.slots.len() as u32;
        self.func.slots.push(SlotRec { size, align, kind: kind as u8, pad: [0; 2] });
        SlotId(row)
    }

    /// An `asm!` block: its template, its operands, its options (`AsmRec`'s bits) and the
    /// ABIs it clobbers, comma-separated.
    pub fn asm_block(
        &mut self,
        template: &[AsmPiece<'_>],
        operands: &[AsmOperand<'_>],
        options: u32,
        clobbers: &str,
    ) -> AsmId {
        let pieces_first = self.func.asm_pieces.len() as u32;
        for piece in template {
            let rec = match *piece {
                AsmPiece::Text(text) => {
                    let first = self.push_text(text);
                    AsmPieceRec { kind: AsmPieceRec::TEXT, a: first, b: text.len() as u32 }
                }
                AsmPiece::Operand { index, modifier } => AsmPieceRec {
                    kind: AsmPieceRec::OPERAND,
                    a: index,
                    b: modifier.map_or(0, u32::from),
                },
            };
            self.func.asm_pieces.push(rec);
        }
        let operands_first = self.func.asm_operands.len() as u32;
        for operand in operands {
            let reg_first = self.push_text(operand.reg);
            self.func.asm_operands.push(AsmOperandRec {
                reg_first,
                reg_len: operand.reg.len() as u32,
                aux: operand.aux,
                kind: operand.kind as u8,
                class: match operand.reg_kind {
                    RegKind::Reg => 0,
                    RegKind::Class => 1,
                },
                pad: [0; 2],
            });
        }
        let clobbers_first = self.push_text(clobbers);
        let row = self.func.asms.len() as u32;
        self.func.asms.push(AsmRec {
            pieces_first,
            pieces_len: template.len() as u32,
            operands_first,
            operands_len: operands.len() as u32,
            options,
            clobbers_first,
            clobbers_len: clobbers.len() as u32,
        });
        AsmId(row)
    }

    /// A named local, for debug info.
    pub fn variable(&mut self, variable: VarRec) {
        self.func.variables.push(variable);
    }

    /// A scope of the body inside `parent` (a scope made before, [`NONE`] for none).
    pub fn scope(&mut self, parent: u32) -> u32 {
        let row = self.func.scope_parents.len() as u32;
        self.func.scope_parents.push(parent);
        row
    }

    /// A body the MIR optimizer inlined, inside `parent` ([`NONE`] for the function's own).
    pub fn inlined(&mut self, parent: u32, call_site: SpanRef) -> u32 {
        let row = self.func.inlined.len() as u32;
        self.func.inlined.push(InlinedRec { parent, call_site });
        row
    }

    /// The location of the instructions made from here: a span, or none.
    pub fn set_loc(&mut self, span: Option<SpanRef>) {
        self.loc = match span {
            None => 0,
            Some(span) => self.span(span) + 1,
        };
    }

    /// [`Builder::set_loc`] of a span in a scope (a `SCOPE_*` word) and an inlined body (a row
    /// of `inlined`, [`NONE`] for the function's own). A function's spans all have a scope
    /// or none do.
    pub fn set_loc_in(&mut self, span: SpanRef, scope: u32, inlined: u32) {
        let new = !self.span_rows.contains_key(&span);
        let row = self.span(span);
        if new {
            self.func.span_scopes.push(scope);
            self.func.span_inlined.push(inlined);
        }
        self.loc = row + 1;
    }

    fn span(&mut self, span: SpanRef) -> u32 {
        if let Some(&row) = self.span_rows.get(&span) {
            return row;
        }
        let row = self.func.spans.len() as u32;
        self.func.spans.push(span);
        self.span_rows.insert(span, row);
        row
    }

    // Blocks.

    /// A block to fill later.
    pub fn create_block(&mut self, kind: BlockKind) -> Block {
        let block = Block(self.places.len() as u32);
        self.places.push(NONE);
        self.kinds.push(kind as u8);
        self.named.push(0);
        block
    }

    /// Starts filling `block`: the current block must have its terminator.
    pub fn begin(&mut self, block: Block) {
        if block.index() >= self.places.len() {
            return self.fail(BuildError::NoSuchBlock { block });
        }
        if self.current != NONE && !self.ended {
            self.fail(BuildError::Unterminated { block: Block(self.current) });
        }
        if self.places[block.index()] != NONE {
            return self.fail(BuildError::Begun { block });
        }
        self.places[block.index()] = self.begun;
        self.begun += 1;
        if let Makes::Code = S::MAKES {
            self.func.block_starts.push(self.func.ops.len() as u32);
            self.func.block_kinds.push(self.kinds[block.index()]);
        }
        self.current = block.0;
        self.params_open = true;
        self.ended = false;
    }

    /// The block being filled, as made.
    pub fn current(&self) -> Option<Block> {
        (self.current != NONE).then_some(Block(self.current))
    }

    /// Whether the current block has its terminator.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// A parameter of the current block, before any of its instructions.
    pub fn param(&mut self, ty: Ty) -> Value {
        self.push(Op::Param, Flags::NONE, ty, 0, |_| {})
    }

    fn check_block(&mut self, block: Block) {
        match self.named.get_mut(block.index()) {
            Some(named) => *named = 1,
            None => self.fail(BuildError::NoSuchBlock { block }),
        }
    }

    /// Appends an instruction, its operands written by `write`.
    fn push(
        &mut self,
        op: Op,
        flags: Flags,
        ty: Ty,
        attrs: u32,
        write: impl FnOnce(&mut Words<'_, A>),
    ) -> Value {
        let value = Value(self.func.tys.len() as u32);
        if let Makes::Requests = S::MAKES {
            self.func.tys.push(ty.0);
            if op.is_terminator() {
                self.ended = true;
            }
            return value;
        }
        if self.current == NONE {
            self.fail(BuildError::NoBlock { op });
        } else if self.ended {
            self.fail(BuildError::AfterEnd { op, block: Block(self.current) });
        }
        if op != Op::Param {
            self.params_open = false;
        } else if !self.params_open {
            self.fail(BuildError::LateParam { block: Block(self.current) });
        }
        let start = self.func.extra.len();
        write(&mut Words { extra: &mut self.func.extra, fixups: &mut self.fixups });
        let data = match op.info().layout {
            Layout::None => {
                self.func.extra.truncate(start);
                0
            }
            Layout::Data(_) => {
                let word = self.func.extra.get(start).copied().unwrap_or(0);
                self.func.extra.truncate(start);
                word
            }
            Layout::Extra(_) => start as u32,
        };
        self.func.ops.push(op as u8);
        self.func.flags.push(flags.0);
        self.func.tys.push(ty.0);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(self.loc);
        if op.is_terminator() {
            self.ended = true;
        }
        value
    }

    fn push_targets(&mut self, targets: &[Block]) {
        for &block in targets {
            self.check_block(block);
        }
    }

    /// The result type of a call of `sig`: none, its one return, or the compound of them.
    pub fn returns(&mut self, sig: SigId) -> Ty {
        let Some(rec) = self.func.sigs.get(sig.index()).copied() else { return Ty::NONE };
        let rets = rec.first as usize + rec.params as usize;
        match rec.rets {
            0 => Ty::NONE,
            1 => Ty(self.func.sig_params[rets].ty),
            n => {
                // alloc: a signature's returns, a few words, in the builder's allocator
                let mut tys = Vec::with_capacity_in(n as usize, self.func.tys.allocator().clone());
                tys.extend(self.func.sig_params[rets..][..n as usize].iter().map(|it| Ty(it.ty)));
                self.compound(&tys)
            }
        }
    }

    // Values.

    /// An integer constant of scalar type `ty`, truncated to its width: in `data` when it
    /// fits, as [`Op::Iconst`] says, else wide.
    pub fn iconst(&mut self, ty: Ty, value: u128) -> Value {
        let bits = ty.bits(self.ptr_bits());
        if !ty.is_scalar() || !ty.is_int() {
            self.fail(BuildError::Operand { op: Op::Iconst, rule: "a type that is not a scalar integer" });
            return self.push(Op::Iconst, Flags::NONE, ty, 0, |w| w.word(0));
        }
        let value = match bits {
            128.. => value,
            bits => value & ((1u128 << bits) - 1),
        };
        if bits <= 32 {
            return self.push(Op::Iconst, Flags::NONE, ty, 0, |w| w.word(value as u32));
        }
        let shift = 128 - bits;
        let signed = ((value << shift) as i128) >> shift;
        match i32::try_from(signed) {
            Ok(small) => self.push(Op::Iconst, Flags::NONE, ty, 0, |w| w.word(small as u32)),
            Err(_) => self.push(Op::IconstWide, Flags::NONE, ty, 0, |w| {
                words_of(value, ty.const_words()).for_each(|it| w.word(it))
            }),
        }
    }

    /// A float constant of type `ty` of the bits `bits`.
    pub fn fconst(&mut self, ty: Ty, bits: u128) -> Value {
        match ty.elem() {
            Scalar::F16 => self.push(Op::Fconst, Flags::NONE, ty, 0, |w| w.word(bits as u16 as u32)),
            Scalar::F32 => self.push(Op::Fconst, Flags::NONE, ty, 0, |w| w.word(bits as u32)),
            Scalar::F64 => self.push(Op::FconstWide, Flags::NONE, ty, 0, |w| {
                words_of(bits as u64 as u128, 2).for_each(|it| w.word(it))
            }),
            Scalar::F128 => self.push(Op::FconstWide, Flags::NONE, ty, 0, |w| {
                words_of(bits, 4).for_each(|it| w.word(it))
            }),
            Scalar::None
            | Scalar::I1
            | Scalar::I8
            | Scalar::I16
            | Scalar::I32
            | Scalar::I64
            | Scalar::I128
            | Scalar::Ptr => {
                self.fail(BuildError::Operand { op: Op::Fconst, rule: "a type that is not a float" });
                self.push(Op::Fconst, Flags::NONE, ty, 0, |w| w.word(0))
            }
        }
    }

    /// A fixed vector constant, its bytes as little-endian words (the bits past the vector's
    /// width 0).
    pub fn vconst(&mut self, ty: Ty, words: &[u32]) -> Value {
        if !matches!(ty.form(), Form::Vector) || words.len() != ty.const_words() as usize {
            self.fail(BuildError::Operand { op: Op::Vconst, rule: "words that are not its vector's" });
        }
        self.push(Op::Vconst, Flags::NONE, ty, 0, |w| words.iter().for_each(|&it| w.word(it)))
    }

    pub fn null(&mut self) -> Value {
        self.push(Op::Null, Flags::NONE, Ty::PTR, 0, |_| {})
    }

    pub fn uninit(&mut self, ty: Ty) -> Value {
        self.push(Op::Uninit, Flags::NONE, ty, 0, |_| {})
    }

    /// An operation of two operands whose result is of their type: integer and float
    /// arithmetic, bitwise operations, shifts, `swizzle`.
    pub fn binary(&mut self, op: Op, flags: Flags, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        self.push(op, flags, ty, 0, |w| {
            w.value(a);
            w.value(b);
        })
    }

    /// An operation of one operand whose result is of its type.
    pub fn unary(&mut self, op: Op, flags: Flags, a: Value) -> Value {
        let ty = self.ty(a);
        self.push(op, flags, ty, 0, |w| w.value(a))
    }

    /// `fma` or `fmuladd`.
    pub fn ternary(&mut self, op: Op, flags: Flags, a: Value, b: Value, c: Value) -> Value {
        let ty = self.ty(a);
        self.push(op, flags, ty, 0, |w| {
            w.value(a);
            w.value(b);
            w.value(c);
        })
    }

    /// An overflow operation (`uadd_overflow` and kin): `{T, i1}` (lane by lane for a vector).
    pub fn overflow(&mut self, op: Op, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        let ty = self.compound(&[ty, ty.with_elem(Scalar::I1)]);
        self.push(op, Flags::NONE, ty, 0, |w| {
            w.value(a);
            w.value(b);
        })
    }

    pub fn icmp(&mut self, cond: IntCond, a: Value, b: Value) -> Value {
        let ty = self.ty(a).with_elem(Scalar::I1);
        self.push(Op::Icmp, Flags::NONE, ty, 0, |w| {
            w.word(cond as u32);
            w.value(a);
            w.value(b);
        })
    }

    pub fn fcmp(&mut self, cond: FloatCond, a: Value, b: Value) -> Value {
        let ty = self.ty(a).with_elem(Scalar::I1);
        self.push(Op::Fcmp, Flags::NONE, ty, 0, |w| {
            w.word(cond as u32);
            w.value(a);
            w.value(b);
        })
    }

    pub fn select(&mut self, flags: Flags, cond: Value, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        self.push(Op::Select, flags, ty, 0, |w| {
            w.value(cond);
            w.value(a);
            w.value(b);
        })
    }

    /// A conversion to `ty`: `zext` to `bitcast`, `ptr_addr`, `expose`, `from_exposed`,
    /// `splat`, `bitmask`, and the one-operand reductions.
    pub fn convert(&mut self, op: Op, flags: Flags, ty: Ty, a: Value) -> Value {
        self.push(op, flags, ty, 0, |w| w.value(a))
    }

    /// `assume`: `cond` is true.
    pub fn assume(&mut self, cond: Value) {
        self.push(Op::Assume, Flags::NONE, Ty::NONE, 0, |w| w.value(cond));
    }

    // Pointers.

    pub fn ptr_add(&mut self, flags: Flags, ptr: Value, offset: Value) -> Value {
        let ty = self.ty(ptr);
        self.push(Op::PtrAdd, flags, ty, 0, |w| {
            w.value(ptr);
            w.value(offset);
        })
    }

    pub fn ptr_with_addr(&mut self, ptr: Value, addr: Value) -> Value {
        self.push(Op::PtrWithAddr, Flags::NONE, Ty::PTR, 0, |w| {
            w.value(ptr);
            w.value(addr);
        })
    }

    pub fn func_addr(&mut self, callee: RefId) -> Value {
        self.push(Op::FuncAddr, Flags::NONE, Ty::PTR, 0, |w| w.word(callee.0))
    }

    pub fn data_addr(&mut self, data: RefId) -> Value {
        self.push(Op::DataAddr, Flags::NONE, Ty::PTR, 0, |w| w.word(data.0))
    }

    pub fn tls_addr(&mut self, tls: RefId) -> Value {
        self.push(Op::TlsAddr, Flags::NONE, Ty::PTR, 0, |w| w.word(tls.0))
    }

    pub fn slot_addr(&mut self, slot: SlotId) -> Value {
        self.push(Op::SlotAddr, Flags::NONE, Ty::PTR, 0, |w| w.word(slot.0))
    }

    pub fn slot_live(&mut self, slot: SlotId) {
        self.push(Op::SlotLive, Flags::NONE, Ty::NONE, 0, |w| w.word(slot.0));
    }

    pub fn slot_dead(&mut self, slot: SlotId) {
        self.push(Op::SlotDead, Flags::NONE, Ty::NONE, 0, |w| w.word(slot.0));
    }

    // Memory.

    pub fn load(&mut self, ty: Ty, flags: Flags, ptr: Value, offset: i32, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        self.push(Op::Load, flags, ty, attrs, |w| {
            w.value(ptr);
            w.word(offset as u32);
        })
    }

    pub fn store(&mut self, flags: Flags, value: Value, ptr: Value, offset: i32, attrs: MemAttrs) {
        let attrs = self.mem(attrs);
        self.push(Op::Store, flags, Ty::NONE, attrs, |w| {
            w.value(value);
            w.value(ptr);
            w.word(offset as u32);
        });
    }

    /// `memcpy`, `memmove` or `memset`.
    pub fn bulk(&mut self, op: Op, flags: Flags, dst: Value, src: Value, len: Value, attrs: MemAttrs) {
        let attrs = self.mem(attrs);
        self.push(op, flags, Ty::NONE, attrs, |w| {
            w.value(dst);
            w.value(src);
            w.value(len);
        });
    }

    pub fn memcmp(&mut self, a: Value, b: Value, len: Value, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        self.push(Op::Memcmp, Flags::NONE, Ty::I32, attrs, |w| {
            w.value(a);
            w.value(b);
            w.value(len);
        })
    }

    pub fn atomic_load(&mut self, ty: Ty, flags: Flags, ptr: Value, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        self.push(Op::AtomicLoad, flags, ty, attrs, |w| w.value(ptr))
    }

    pub fn atomic_store(&mut self, flags: Flags, value: Value, ptr: Value, attrs: MemAttrs) {
        let attrs = self.mem(attrs);
        self.push(Op::AtomicStore, flags, Ty::NONE, attrs, |w| {
            w.value(value);
            w.value(ptr);
        });
    }

    pub fn atomic_rmw(&mut self, rmw: AtomicRmw, flags: Flags, ptr: Value, value: Value, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        let ty = self.ty(value);
        self.push(Op::AtomicRmw, flags, ty, attrs, |w| {
            w.word(rmw as u32);
            w.value(ptr);
            w.value(value);
        })
    }

    pub fn cmpxchg(&mut self, flags: Flags, ptr: Value, expected: Value, new: Value, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        let ty = self.ty(expected);
        let ty = self.compound(&[ty, Ty::I1]);
        self.push(Op::Cmpxchg, flags, ty, attrs, |w| {
            w.value(ptr);
            w.value(expected);
            w.value(new);
        })
    }

    pub fn fence(&mut self, flags: Flags) {
        self.push(Op::Fence, flags, Ty::NONE, 0, |_| {});
    }

    // Vectors.

    pub fn extract(&mut self, vector: Value, lane: u32) -> Value {
        let ty = self.ty(vector).lane();
        self.push(Op::Extract, Flags::NONE, ty, 0, |w| {
            w.value(vector);
            w.word(lane);
        })
    }

    pub fn insert(&mut self, vector: Value, value: Value, lane: u32) -> Value {
        let ty = self.ty(vector);
        self.push(Op::Insert, Flags::NONE, ty, 0, |w| {
            w.value(vector);
            w.value(value);
            w.word(lane);
        })
    }

    /// The lanes `mask` names of `a` and `b` concatenated, as a vector of `ty`.
    pub fn shuffle(&mut self, ty: Ty, a: Value, b: Value, mask: &[u32]) -> Value {
        self.push(Op::Shuffle, Flags::NONE, ty, 0, |w| {
            w.value(a);
            w.value(b);
            w.word(mask.len() as u32);
            mask.iter().for_each(|&it| w.word(it));
        })
    }

    /// `reduce_fadd` or `reduce_fmul`: `start`, then the vector's lanes.
    pub fn reduce_ordered(&mut self, op: Op, flags: Flags, start: Value, vector: Value) -> Value {
        let ty = self.ty(start);
        self.push(op, flags, ty, 0, |w| {
            w.value(start);
            w.value(vector);
        })
    }

    /// `masked_load` or `gather` of `ty`: the address(es), the mask and the lanes where it is
    /// false.
    pub fn masked_load(&mut self, op: Op, ty: Ty, ptr: Value, mask: Value, passthru: Value, attrs: MemAttrs) -> Value {
        let attrs = self.mem(attrs);
        self.push(op, Flags::NONE, ty, attrs, |w| {
            w.value(ptr);
            w.value(mask);
            w.value(passthru);
        })
    }

    /// `masked_store` or `scatter`: the value, the address(es) and the mask.
    pub fn masked_store(&mut self, op: Op, value: Value, ptr: Value, mask: Value, attrs: MemAttrs) {
        let attrs = self.mem(attrs);
        self.push(op, Flags::NONE, Ty::NONE, attrs, |w| {
            w.value(value);
            w.value(ptr);
            w.value(mask);
        });
    }

    // Calls.

    pub fn call(&mut self, flags: Flags, callee: RefId, sig: SigId, args: &[Value]) -> Value {
        let ty = self.returns(sig);
        self.push(Op::Call, flags, ty, 0, |w| {
            w.word(callee.0);
            w.word(sig.0);
            w.values(args);
        })
    }

    pub fn call_indirect(&mut self, flags: Flags, callee: Value, sig: SigId, args: &[Value]) -> Value {
        let ty = self.returns(sig);
        self.push(Op::CallIndirect, flags, ty, 0, |w| {
            w.value(callee);
            w.word(sig.0);
            w.values(args);
        })
    }

    pub fn target_intrinsic(&mut self, intrinsic: IntrinsicId, args: &[Value]) -> Value {
        let sig = self.func.intrinsics.get(intrinsic.index()).map_or(SigId(NONE), |it| SigId(it.sig));
        let ty = self.returns(sig);
        self.push(Op::TargetIntrinsic, Flags::NONE, ty, 0, |w| {
            w.word(intrinsic.0);
            w.values(args);
        })
    }

    /// An `asm!` block of outputs `ty` (none, one, or a compound in operand order).
    pub fn asm(&mut self, asm: AsmId, ty: Ty, inputs: &[Value]) -> Value {
        self.push(Op::Asm, Flags::NONE, ty, 0, |w| {
            w.word(asm.0);
            w.values(inputs);
        })
    }

    /// Member `k` of compound value `value`.
    pub fn project(&mut self, value: Value, k: u32) -> Value {
        let ty = self.ty(value);
        let ty = self.members(ty).get(k as usize).map_or(Ty::NONE, |&it| Ty(it));
        self.push(Op::Project, Flags::NONE, ty, 0, |w| {
            w.value(value);
            w.word(k);
        })
    }

    /// `frame_address`, `return_address` or `stack_pointer`.
    pub fn machine(&mut self, op: Op) -> Value {
        self.push(op, Flags::NONE, Ty::PTR, 0, |_| {})
    }

    // Ends of blocks.

    pub fn jump(&mut self, to: To<'_>) {
        self.check_block(to.0);
        self.push(Op::Jump, Flags::NONE, Ty::NONE, 0, |w| w.target(to));
    }

    pub fn br(&mut self, flags: Flags, cond: Value, then: To<'_>, otherwise: To<'_>) {
        self.push_targets(&[then.0, otherwise.0]);
        self.push(Op::Br, flags, Ty::NONE, 0, |w| {
            w.value(cond);
            w.target(then);
            w.target(otherwise);
        });
    }

    /// `switch` on the integer `scrutinee`: each case's value (truncated to its width) and
    /// target, then `default`.
    pub fn switch(&mut self, scrutinee: Value, default: To<'_>, cases: &[(u128, To<'_>)]) {
        self.check_block(default.0);
        for (_, to) in cases {
            self.check_block(to.0);
        }
        let words = self.ty(scrutinee).const_words();
        self.push(Op::Switch, Flags::NONE, Ty::NONE, 0, |w| {
            w.value(scrutinee);
            w.target(default);
            w.word(cases.len() as u32);
            for &(value, to) in cases {
                words_of(value, words).for_each(|it| w.word(it));
                w.target(to);
            }
        });
    }

    pub fn ret(&mut self, values: &[Value]) {
        self.push(Op::Return, Flags::NONE, Ty::NONE, 0, |w| w.values(values));
    }

    pub fn unreachable(&mut self) {
        self.push(Op::Unreachable, Flags::NONE, Ty::NONE, 0, |_| {});
    }

    pub fn trap(&mut self, code: u32) {
        self.push(Op::Trap, Flags::NONE, Ty::NONE, 0, |w| w.word(code));
    }

    pub fn resume(&mut self, exception: Value) {
        self.push(Op::Resume, Flags::NONE, Ty::NONE, 0, |w| w.value(exception));
    }

    /// A call that may unwind to `unwind` (a landing block), returning to `normal`, whose
    /// one predecessor it must be. Its value is the call's result, usable from `normal` on.
    pub fn invoke(&mut self, flags: Flags, callee: RefId, sig: SigId, args: &[Value], normal: Block, unwind: Block) -> Value {
        self.push_targets(&[normal, unwind]);
        let ty = self.returns(sig);
        self.push(Op::Invoke, flags, ty, 0, |w| {
            w.word(callee.0);
            w.word(sig.0);
            w.block(normal);
            w.block(unwind);
            w.values(args);
        })
    }

    pub fn invoke_indirect(&mut self, flags: Flags, callee: Value, sig: SigId, args: &[Value], normal: Block, unwind: Block) -> Value {
        self.push_targets(&[normal, unwind]);
        let ty = self.returns(sig);
        self.push(Op::InvokeIndirect, flags, ty, 0, |w| {
            w.value(callee);
            w.word(sig.0);
            w.block(normal);
            w.block(unwind);
            w.values(args);
        })
    }

    pub fn tail_call(&mut self, flags: Flags, callee: RefId, sig: SigId, args: &[Value]) {
        self.push(Op::TailCall, flags, Ty::NONE, 0, |w| {
            w.word(callee.0);
            w.word(sig.0);
            w.values(args);
        });
    }

    pub fn tail_call_indirect(&mut self, flags: Flags, callee: Value, sig: SigId, args: &[Value]) {
        self.push(Op::TailCallIndirect, flags, Ty::NONE, 0, |w| {
            w.value(callee);
            w.word(sig.0);
            w.values(args);
        });
    }

    /// An `asm!` block with labels: falls through to `fallthrough` (whose one predecessor it
    /// must be, its outputs `ty` usable from there on) or jumps to a label.
    pub fn asm_br(&mut self, asm: AsmId, ty: Ty, fallthrough: Block, labels: &[Block], inputs: &[Value]) -> Value {
        self.push_targets(&[fallthrough]);
        self.push_targets(labels);
        self.push(Op::AsmBr, Flags::NONE, ty, 0, |w| {
            w.word(asm.0);
            w.block(fallthrough);
            w.word(labels.len() as u32);
            labels.iter().for_each(|&it| w.block(it));
            w.values(inputs);
        })
    }

    fn check_finish(&mut self) -> Result<(), BuildError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        if self.current != NONE && !self.ended {
            return Err(BuildError::Unterminated { block: Block(self.current) });
        }
        if self.signature.is_none() {
            return Err(BuildError::NoSignature);
        }
        Ok(())
    }
}

impl<A: Allocator + Clone> Builder<A, Full> {
    /// The function: every block an edge names begun, every edge in layout order, checked by
    /// the verifier in debug builds.
    pub fn finish(self) -> Result<Func<A>, BuildError> {
        self.finish_as(Check::Debug)
    }

    /// [`Builder::finish`], verified in every build: the check mode.
    pub fn finish_checked(self) -> Result<Func<A>, BuildError> {
        self.finish_as(Check::Always)
    }

    fn finish_as(mut self, check: Check) -> Result<Func<A>, BuildError> {
        self.check_finish()?;
        if let Some(block) = (0..self.named.len())
            .find(|&b| self.named[b] != 0 && self.places[b] == NONE)
        {
            return Err(BuildError::Unbegun { block: Block(block as u32) });
        }
        for &at in self.fixups.iter() {
            let word = &mut self.func.extra[at as usize];
            *word = self.places[*word as usize];
        }
        row(self.func.ops.len(), "instructions")?;
        let verify_now = match check {
            Check::Always => true,
            Check::Debug => cfg!(debug_assertions),
        };
        if verify_now {
            verify::verify(self.func.view()).map_err(BuildError::Verify)?;
        }
        Ok(self.func)
    }
}

impl<A: Allocator + Clone> Builder<A, RequestsOnly> {
    /// What the function refers to.
    pub fn finish(mut self) -> Result<Requests<A>, BuildError> {
        self.check_finish()?;
        Ok(Requests(self.func))
    }
}

/// When [`Builder::finish`] runs the verifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Check {
    Debug,
    Always,
}

/// Where an instruction's operands are written.
struct Words<'b, A: Allocator> {
    extra: &'b mut Vec<u32, A>,
    fixups: &'b mut Vec<u32, A>,
}

impl<A: Allocator> Words<'_, A> {
    fn word(&mut self, word: u32) {
        self.extra.push(word);
    }

    fn value(&mut self, value: Value) {
        self.extra.push(value.0);
    }

    fn block(&mut self, block: Block) {
        self.fixups.push(self.extra.len() as u32);
        self.extra.push(block.0);
    }

    fn target(&mut self, (block, args): To<'_>) {
        self.block(block);
        self.values(args);
    }

    fn values(&mut self, values: &[Value]) {
        self.extra.push(values.len() as u32);
        self.extra.extend(values.iter().map(|it| it.0));
    }
}
