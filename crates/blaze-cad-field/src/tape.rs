//! THE TAPE: a field as a flat SSA program of fixed-size instructions, canonical, and the
//! register program the interpreters run.
//!
//! # Form
//!
//! An [`Instr`] is 24 bytes: an [`Op`] byte, four operand indices (`u32`, each an EARLIER
//! instruction; unused ones are zero) and one `extra` word (a constant pool index or a table
//! index). Constants live in a pool of f64 (a `Const` reads one, a `Row` four, a `BoxBound`
//! six, a `PowConst` its exponent); path and grid tables live in a data block. The
//! register program beside it is 16 bytes an instruction (`u16` registers), made by one forward
//! scan with last uses (a value's register is freed after its last reader, freed registers are
//! reused last-in first-out), so an interpreter's working set is the number of values live at
//! once, not the tape's length.
//!
//! # Canonical
//!
//! Every tape this crate makes (by [`crate::compile`], [`crate::simplify`] and
//! [`crate::section`]) is the output of ONE emitter (a decoded tape is what was encoded, its
//! digest taken again from its bytes): a depth-first post-order walk from the
//! root, operands in order, with hash-consing (one instruction per distinct op and operands),
//! constant folding (an op of constants becomes the constant the point interpreter would compute)
//! and the bit-exact identities (`x * 1`, `x / 1`, `x - 0`, `-(-x)`, `min(x, x)`, `max(x, x)`,
//! `select(.., c, c)`). The walk visits only what the root reaches, so the emitter is also the
//! dead-code pass. Two tapes of one DAG are therefore equal byte for byte however the DAG was
//! built or what was simplified away around it, and [`Tape::digest`] is a key: it is what a
//! region of a mesh is keyed by (the simplified tape's digest, the box and the tolerance).
//!
//! # Cost
//!
//! Emitting is linear in the instructions reached (one hash probe each); register allocation is
//! one forward pass; the digest one BLAKE3 pass over the encoded bytes. A rewrite that could
//! orphan an instruction already emitted (a fold) marks the emitter, and only then is a second
//! emitting pass run to drop it.
//!
//! # Encoding
//!
//! Little-endian, in this order: `BCFT`, the schema digest (32 bytes), the level byte and three
//! zero bytes, the root, the counts (instructions, pool, tables, data, each `u32`), the box (six
//! f64), the instructions (24 bytes each: op, three zero bytes, four operands, extra), the pool
//! (f64 bits), the tables (kind byte, three zero bytes, start, length, eight f64 of head) and
//! the data (f64 bits). The DIGEST covers everything but the box: the box is where the root's
//! solid lies, metadata a simplified tape inherits from its whole model, and a region key must
//! not change when a part far away grows. [`Tape::decode`] checks every byte before the tape is
//! used.

use blaze_blake3::Hasher;
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::Planted;
use blaze_own::{AVec, Alloc, Vec};

use crate::Limits;
use crate::error::{CompileError, DecodeError};
use crate::level::Level;
use crate::node::Digest;
use crate::plant::CadPlant;
use crate::profile::SEG_WIDTH;

/// An instruction's operation.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    /// The query point's coordinates.
    X = 0,
    Y = 1,
    Z = 2,
    /// `pool[extra]`.
    Const = 3,
    Neg = 4,
    Abs = 5,
    Sqrt = 6,
    Square = 7,
    Sin = 8,
    Cos = 9,
    /// Halves away from zero.
    Round = 10,
    Add = 11,
    Sub = 12,
    Mul = 13,
    Div = 14,
    /// `if b < a { b } else { a }`: a choice site.
    Min = 15,
    /// `if a < b { b } else { a }`: a choice site.
    Max = 16,
    /// `atan2(a, b)`: `a` is `y`.
    Atan2 = 17,
    /// `a ^ pool[extra]`, `a >= 0`.
    PowConst = 18,
    /// `if a < b { c } else { d }`: a choice site.
    Select = 19,
    /// `k0 a + k1 b + k2 c + k3` with `k = pool[extra..extra + 4]`, summed in that order: a
    /// local coordinate.
    Row = 20,
    /// `max_i(|p_i - centre_i| - half_i)` of `p = (a, b, c)`, centre and half extents at
    /// `pool[extra..extra + 6]`: a bound of the distance to the box.
    BoxBound = 21,
    /// The signed distance at `(a, b)` to the closed path of table `extra`.
    Path2 = 22,
    /// The solved distance grid of table `extra` at `(a, b, c)`.
    Grid3 = 23,
}

const OPS: [Op; 24] = [
    Op::X,
    Op::Y,
    Op::Z,
    Op::Const,
    Op::Neg,
    Op::Abs,
    Op::Sqrt,
    Op::Square,
    Op::Sin,
    Op::Cos,
    Op::Round,
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::Div,
    Op::Min,
    Op::Max,
    Op::Atan2,
    Op::PowConst,
    Op::Select,
    Op::Row,
    Op::BoxBound,
    Op::Path2,
    Op::Grid3,
];

impl Op {
    /// The op of a byte.
    pub const fn from_byte(b: u8) -> Option<Op> {
        if (b as usize) < OPS.len() { Some(OPS[b as usize]) } else { None }
    }

    /// The op's name.
    pub const fn name(self) -> &'static str {
        match self {
            Op::X => "x",
            Op::Y => "y",
            Op::Z => "z",
            Op::Const => "const",
            Op::Neg => "neg",
            Op::Abs => "abs",
            Op::Sqrt => "sqrt",
            Op::Square => "square",
            Op::Sin => "sin",
            Op::Cos => "cos",
            Op::Round => "round",
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::Div => "div",
            Op::Min => "min",
            Op::Max => "max",
            Op::Atan2 => "atan2",
            Op::PowConst => "powc",
            Op::Select => "select",
            Op::Row => "row",
            Op::BoxBound => "boxbound",
            Op::Path2 => "path2",
            Op::Grid3 => "grid3",
        }
    }

    /// How many value operands it reads (`a`, `b`, `c`, `d` in order).
    pub const fn arity(self) -> usize {
        match self {
            Op::X | Op::Y | Op::Z | Op::Const => 0,
            Op::Neg | Op::Abs | Op::Sqrt | Op::Square | Op::Sin | Op::Cos | Op::Round | Op::PowConst => 1,
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Min | Op::Max | Op::Atan2 | Op::Path2 => 2,
            Op::Row | Op::BoxBound | Op::Grid3 => 3,
            Op::Select => 4,
        }
    }

    /// How many pool constants `extra` names.
    pub const fn pool(self) -> usize {
        match self {
            Op::Const | Op::PowConst => 1,
            Op::Row => 4,
            Op::BoxBound => 6,
            _ => 0,
        }
    }

    /// Whether `extra` names a table.
    pub const fn table(self) -> bool {
        matches!(self, Op::Path2 | Op::Grid3)
    }

    /// Whether it is a choice site (`eval_box` records a [`crate::Choice`] for it).
    pub const fn choice(self) -> bool {
        matches!(self, Op::Min | Op::Max | Op::Select)
    }
}

/// One SSA instruction (24 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Instr {
    pub op: Op,
    /// Earlier instructions read; unused ones are zero.
    pub args: [u32; 4],
    /// A pool index or a table index; zero when the op has neither.
    pub extra: u32,
}

/// The size of an [`Instr`] in the encoding.
pub const INSTR_BYTES: usize = 24;

/// What a table holds.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum TableKind {
    /// Path rows, `SEG_WIDTH` values each; the head is unused (zeros).
    Path2 = 0,
    /// Grid values, `i` fastest; the head is `[origin x, y, z, step, band, nx, ny, nz]`.
    Grid3 = 1,
}

/// A table: its kind, its head and its rows `data[start..start + len]`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Table {
    pub kind: TableKind,
    pub head: [f64; 8],
    pub start: u32,
    pub len: u32,
}

/// Coarse min-max block edge length (`4 x 4 x 4` voxels per level-1 block,
/// `16 x 16 x 16` voxels per level-2 superblock).
pub(crate) const GRID_BLOCK: usize = 4;
pub(crate) const GRID_SUPER: usize = GRID_BLOCK * GRID_BLOCK;

/// Coarse min-max summary metadata for one [`Table`] (zeros for `Path2`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridSummary {
    pub b1_start: u32,
    pub b1_dims: [u32; 3],
    pub b2_start: u32,
    pub b2_dims: [u32; 3],
}

/// One register instruction (16 bytes): the SSA instruction with registers for indices.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RInstr {
    pub op: Op,
    pub out: u16,
    pub args: [u16; 4],
    pub extra: u32,
}

/// A compiled field (see the module).
pub struct Tape<'a> {
    pub(crate) instrs: AVec<'a, Instr>,
    pub(crate) pool: AVec<'a, f64>,
    pub(crate) tables: AVec<'a, Table>,
    pub(crate) data: AVec<'a, f64>,
    pub(crate) grid_blocks: AVec<'a, [f64; 2]>,
    pub(crate) grid_summaries: AVec<'a, GridSummary>,
    pub(crate) root: u32,
    pub(crate) level: Level,
    pub(crate) aabb: Aabb3,
    pub(crate) sites: u32,
    pub(crate) prog: AVec<'a, RInstr>,
    pub(crate) regs: u32,
    pub(crate) out: u16,
    pub(crate) digest: Digest,
    pub(crate) limits: Limits,
}

/// The schema text the schema digest is taken of: the op table and the layout. Any change to
/// either changes it, and a tape written under another schema is refused.
const SCHEMA: &str = "blaze-cad-field tape v1: instr 24 bytes (op u8, 3 zero, a b c d u32, extra u32) LE; \
ops x y z const neg abs sqrt square sin cos round add sub mul div min max atan2 powc select row boxbound path2 grid3; \
pool f64; table (kind u8, 3 zero, start u32, len u32, head 8 f64); path rows 8 f64 (x0 y0 x1 y1 cx cy r bulge); \
grid head (ox oy oz step band nx ny nz), values i fastest; min = b<a?b:a; max = a<b?b:a; select = a<b?c:d; \
row sums k0a+k1b then +k2c then +k3; boxbound max over x y z of |p-c|-h";

/// The schema's digest.
pub fn schema_digest() -> Digest {
    let mut h = Hasher::new_derive_key("blaze-cad-field 2026-11-20 tape schema");
    h.update(SCHEMA.as_bytes());
    h.finalize()
}

/// Where encoded bytes go: a byte vector, or a hasher.
pub(crate) trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for AVec<'_, u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl Sink for Hasher {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

/// What a write includes.
#[derive(Clone, Copy)]
struct Writes {
    aabb: bool,
    pool: bool,
    pool_as_f32: bool,
}

const MAGIC: [u8; 4] = *b"BCFT";

impl<'a> Tape<'a> {
    /// Instructions.
    pub fn len(&self) -> usize {
        self.instrs.len()
    }

    /// Whether there are none (never, for a tape this crate made).
    pub fn is_empty(&self) -> bool {
        self.instrs.is_empty()
    }

    /// The SSA instructions.
    pub fn instrs(&self) -> &[Instr] {
        &self.instrs
    }

    /// The root's index.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// The tape's digest: BLAKE3 of its encoding without the box (see the module).
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The level of the field it computes.
    pub fn level(&self) -> Level {
        self.level
    }

    /// The box the root's solid lies in (world frame).
    pub fn aabb(&self) -> Aabb3 {
        self.aabb
    }

    /// Choice sites.
    pub fn sites(&self) -> u32 {
        self.sites
    }

    /// Registers the interpreters need.
    pub fn registers(&self) -> u32 {
        self.regs
    }

    /// The limits it was made under (a simplified tape keeps them).
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The values of its `Const` instructions, in order.
    pub fn consts(&self) -> impl Iterator<Item = f64> + '_ {
        self.instrs.iter().filter(|i| i.op == Op::Const).map(|i| self.pool[i.extra as usize])
    }

    /// Whether a `Const` instruction holds exactly `v`.
    pub fn holds_const(&self, v: f64) -> bool {
        self.consts().any(|c| c.to_bits() == v.to_bits())
    }

    /// Appends the encoding to `out`.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        self.encode_with(out, blaze_own::planted::NoPlant);
    }

    /// [`Tape::encode`] through the plant seam.
    pub fn encode_with(&self, out: &mut AVec<'_, u8>, plant: impl Planted<CadPlant>) {
        let parts = Writes { aabb: true, pool: true, pool_as_f32: plant.has(CadPlant::EncodeConstsAsF32) };
        self.write(out, parts);
    }

    fn write<S: Sink>(&self, s: &mut S, parts: Writes) {
        s.put(&MAGIC);
        s.put(&schema_digest());
        s.put(&[self.level as u8, 0, 0, 0]);
        s.put(&self.root.to_le_bytes());
        for n in [self.instrs.len(), self.pool.len(), self.tables.len(), self.data.len()] {
            s.put(&(n as u32).to_le_bytes());
        }
        if parts.aabb {
            for v in self.aabb.lo.to_array().iter().chain(self.aabb.hi.to_array().iter()) {
                s.put(&v.to_bits().to_le_bytes());
            }
        }
        for i in self.instrs.iter() {
            s.put(&[i.op as u8, 0, 0, 0]);
            for a in i.args {
                s.put(&a.to_le_bytes());
            }
            s.put(&i.extra.to_le_bytes());
        }
        if parts.pool {
            for v in self.pool.iter() {
                let v = if parts.pool_as_f32 { f64::from(*v as f32) } else { *v };
                s.put(&v.to_bits().to_le_bytes());
            }
        }
        for t in self.tables.iter() {
            s.put(&[t.kind as u8, 0, 0, 0]);
            s.put(&t.start.to_le_bytes());
            s.put(&t.len.to_le_bytes());
            for v in t.head {
                s.put(&v.to_bits().to_le_bytes());
            }
        }
        for v in self.data.iter() {
            s.put(&v.to_bits().to_le_bytes());
        }
    }

    /// The tape `bytes` encode, every byte checked, in `alloc`, under `limits`.
    pub fn decode(bytes: &[u8], alloc: Alloc<'a>, limits: Limits) -> Result<Tape<'a>, DecodeError> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err(DecodeError::Magic);
        }
        if r.take(32)? != schema_digest() {
            return Err(DecodeError::Schema);
        }
        let head = r.take(4)?;
        let level = match head[0] {
            0 => Level::Bound,
            1 => Level::Outside,
            2 => Level::Inside,
            3 => Level::Exact,
            byte => return Err(DecodeError::Level { byte }),
        };
        let root = r.u32()?;
        let (n, np, nt, nd) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
        // The instructions are bounded by the limit; the pool, the tables and the data by the
        // bytes actually present (each entry is at least 8 bytes), so a lying count stops at
        // `Short` before anything is reserved for it.
        let most = u64::from(limits.most_instrs);
        if u64::from(n) > most {
            return Err(DecodeError::TooLong { most });
        }
        let room = (bytes.len() as u64) / 8;
        for c in [n, np, nt, nd] {
            if u64::from(c) > room {
                return Err(DecodeError::Short { need: u64::from(c) * 8, have: bytes.len() as u64 });
            }
        }
        let mut f = [0.0; 6];
        for v in f.iter_mut() {
            *v = f64::from_bits(r.u64()?);
        }
        let aabb = Aabb3::new(Vec3::new(f[0], f[1], f[2]), Vec3::new(f[3], f[4], f[5]));
        let mut instrs: AVec<'a, Instr> = Vec::with_capacity_in(n as usize, alloc);
        for at in 0..n {
            let b = r.take(4)?;
            let Some(op) = Op::from_byte(b[0]) else {
                return Err(DecodeError::Op { at: u64::from(at), byte: b[0] });
            };
            let mut args = [0u32; 4];
            for (k, a) in args.iter_mut().enumerate() {
                *a = r.u32()?;
                let ok = if k < op.arity() { *a < at } else { *a == 0 };
                if !ok || b[1..4] != [0, 0, 0] {
                    return Err(DecodeError::Operand { at: u64::from(at), operand: *a });
                }
            }
            let extra = r.u32()?;
            let ok_extra = if op.pool() > 0 {
                u64::from(extra) + op.pool() as u64 <= u64::from(np)
            } else if op.table() {
                extra < nt
            } else {
                extra == 0
            };
            if !ok_extra {
                return Err(if op.table() { DecodeError::Table { at: u64::from(at) } } else { DecodeError::Pool { at: u64::from(at) } });
            }
            instrs.push(Instr { op, args, extra });
        }
        if root >= n {
            return Err(DecodeError::Root { root, len: u64::from(n) });
        }
        let mut pool: AVec<'a, f64> = Vec::with_capacity_in(np as usize, alloc);
        for _ in 0..np {
            pool.push(f64::from_bits(r.u64()?));
        }
        let mut tables: AVec<'a, Table> = Vec::with_capacity_in(nt as usize, alloc);
        for at in 0..nt {
            let b = r.take(4)?;
            let kind = match b[0] {
                0 => TableKind::Path2,
                1 => TableKind::Grid3,
                _ => return Err(DecodeError::Table { at: u64::from(at) }),
            };
            let (start, len) = (r.u32()?, r.u32()?);
            let mut head = [0.0; 8];
            for v in head.iter_mut() {
                *v = f64::from_bits(r.u64()?);
            }
            let fits = u64::from(start) + u64::from(len) <= u64::from(nd);
            let shaped = match kind {
                TableKind::Path2 => len as usize % SEG_WIDTH == 0,
                TableKind::Grid3 => {
                    let dims = [head[5], head[6], head[7]];
                    let whole = dims.iter().all(|d| *d >= 2.0 && *d <= 1e9 && blaze_math::floor(*d) == *d);
                    whole && dims.iter().map(|d| *d as u64).product::<u64>() == u64::from(len) && head[3] > 0.0
                }
            };
            if b[1..4] != [0, 0, 0] || !fits || !shaped {
                return Err(DecodeError::Table { at: u64::from(at) });
            }
            tables.push(Table { kind, head, start, len });
        }
        for (at, i) in instrs.iter().enumerate() {
            let want = match i.op {
                Op::Path2 => Some(TableKind::Path2),
                Op::Grid3 => Some(TableKind::Grid3),
                _ => None,
            };
            if let Some(k) = want
                && tables[i.extra as usize].kind != k
            {
                return Err(DecodeError::Table { at: at as u64 });
            }
        }
        let mut data: AVec<'a, f64> = Vec::with_capacity_in(nd as usize, alloc);
        for _ in 0..nd {
            data.push(f64::from_bits(r.u64()?));
        }
        if r.at != bytes.len() {
            return Err(DecodeError::Trailing { extra: (bytes.len() - r.at) as u64 });
        }
        let parts = Parts { instrs, pool, tables, data, root };
        finish(parts, level, aabb, limits, alloc, blaze_own::planted::NoPlant).map_err(|e| match e {
            CompileError::TooWide { most } => DecodeError::TooWide { most },
            _ => DecodeError::TooLong { most },
        })
    }
}

/// Reads little-endian values, refusing a short read.
struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], DecodeError> {
        let end = self.at + n;
        if end > self.bytes.len() {
            return Err(DecodeError::Short { need: end as u64, have: self.bytes.len() as u64 });
        }
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

/// The parts of a tape before its register program and digest.
pub(crate) struct Parts<'a> {
    pub instrs: AVec<'a, Instr>,
    pub pool: AVec<'a, f64>,
    pub tables: AVec<'a, Table>,
    pub data: AVec<'a, f64>,
    pub root: u32,
}

/// Builds the register program, counts the sites and takes the digest.
pub(crate) fn finish<'a>(
    parts: Parts<'a>,
    level: Level,
    aabb: Aabb3,
    limits: Limits,
    alloc: Alloc<'a>,
    plant: impl Planted<CadPlant>,
) -> Result<Tape<'a>, CompileError> {
    let Parts { instrs, pool, tables, data, root } = parts;
    let n = instrs.len();
    // Last uses: a value is freed after the last instruction reading it; the root never is.
    let mut last: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    for j in 0..n {
        last.push(j as u32);
    }
    for (j, i) in instrs.iter().enumerate() {
        for a in &i.args[..i.op.arity()] {
            last[*a as usize] = j as u32;
        }
    }
    last[root as usize] = u32::MAX;
    let mut reg: AVec<'a, u16> = Vec::with_capacity_in(n, alloc);
    let mut free: AVec<'a, u16> = Vec::new_in(alloc);
    let mut prog: AVec<'a, RInstr> = Vec::with_capacity_in(n, alloc);
    let mut next: u32 = 0;
    let mut sites = 0u32;
    for (j, i) in instrs.iter().enumerate() {
        let k = i.op.arity();
        let mut args = [0u16; 4];
        for (slot, a) in args.iter_mut().zip(i.args[..k].iter()) {
            *slot = reg[*a as usize];
        }
        for (x, a) in i.args[..k].iter().enumerate() {
            if last[*a as usize] == j as u32 && !i.args[..x].contains(a) {
                free.push(reg[*a as usize]);
            }
        }
        let out = match free.pop() {
            Some(r) => r,
            None => {
                if next >= limits.most_registers.min(65_535) {
                    return Err(CompileError::TooWide { most: u64::from(limits.most_registers) });
                }
                next += 1;
                (next - 1) as u16
            }
        };
        reg.push(out);
        prog.push(RInstr { op: i.op, out, args, extra: i.extra });
        if i.op.choice() {
            sites += 1;
        }
    }
    let out = reg[root as usize];
    // Deterministic 2-level coarse min-max block summaries (4^3 and 16^3) for `Grid3` tables.
    let mut grid_blocks: AVec<'a, [f64; 2]> = Vec::new_in(alloc);
    let mut grid_summaries: AVec<'a, GridSummary> = Vec::with_capacity_in(tables.len(), alloc);
    for tb in tables.iter() {
        if tb.kind != TableKind::Grid3 {
            grid_summaries.push(GridSummary { b1_start: 0, b1_dims: [0; 3], b2_start: 0, b2_dims: [0; 3] });
            continue;
        }
        let gn = [tb.head[5] as usize, tb.head[6] as usize, tb.head[7] as usize];
        let vals = &data[tb.start as usize..(tb.start + tb.len) as usize];
        let b1 = [gn[0].div_ceil(GRID_BLOCK), gn[1].div_ceil(GRID_BLOCK), gn[2].div_ceil(GRID_BLOCK)];
        let b1_start = grid_blocks.len() as u32;
        let b1_len = b1[0] * b1[1] * b1[2];
        grid_blocks.reserve(b1_len);
        for _ in 0..b1_len {
            grid_blocks.push([f64::INFINITY, f64::NEG_INFINITY]);
        }
        for k in 0..gn[2] {
            let bk = k / GRID_BLOCK;
            for j in 0..gn[1] {
                let bj = j / GRID_BLOCK;
                let row_ofs = gn[0] * (j + gn[1] * k);
                let b_row = b1_start as usize + b1[0] * (bj + b1[1] * bk);
                for i in 0..gn[0] {
                    let v = vals[row_ofs + i];
                    let slot = &mut grid_blocks[b_row + (i / GRID_BLOCK)];
                    slot[0] = slot[0].min(v);
                    slot[1] = slot[1].max(v);
                }
            }
        }
        let b2 = [b1[0].div_ceil(GRID_BLOCK), b1[1].div_ceil(GRID_BLOCK), b1[2].div_ceil(GRID_BLOCK)];
        let b2_start = grid_blocks.len() as u32;
        let b2_len = b2[0] * b2[1] * b2[2];
        grid_blocks.reserve(b2_len);
        for _ in 0..b2_len {
            grid_blocks.push([f64::INFINITY, f64::NEG_INFINITY]);
        }
        for bk in 0..b1[2] {
            let sk = bk / GRID_BLOCK;
            for bj in 0..b1[1] {
                let sj = bj / GRID_BLOCK;
                let b1_row = b1_start as usize + b1[0] * (bj + b1[1] * bk);
                let b2_row = b2_start as usize + b2[0] * (sj + b2[1] * sk);
                for bi in 0..b1[0] {
                    let pair = grid_blocks[b1_row + bi];
                    let slot = &mut grid_blocks[b2_row + (bi / GRID_BLOCK)];
                    slot[0] = slot[0].min(pair[0]);
                    slot[1] = slot[1].max(pair[1]);
                }
            }
        }
        grid_summaries.push(GridSummary {
            b1_start,
            b1_dims: [b1[0] as u32, b1[1] as u32, b1[2] as u32],
            b2_start,
            b2_dims: [b2[0] as u32, b2[1] as u32, b2[2] as u32],
        });
    }
    let mut tape = Tape {
        instrs,
        pool,
        tables,
        data,
        grid_blocks,
        grid_summaries,
        root,
        level,
        aabb,
        sites,
        prog,
        regs: next,
        out,
        digest: [0; 32],
        limits,
    };
    let mut h = Hasher::new_derive_key("blaze-cad-field 2026-11-20 tape");
    let skip = plant.has(CadPlant::TapeDigestSkipsConsts);
    tape.write(&mut h, Writes { aabb: false, pool: !skip, pool_as_f32: false });
    tape.digest = h.finalize();
    Ok(tape)
}

/// What the emitter does with one source instruction.
#[derive(Clone, Copy)]
pub(crate) enum Rw {
    /// Copy it.
    Keep,
    /// It is the value of the source instruction given.
    Alias(u32),
    /// A coordinate input replaced by the row `k0 x + k1 y + k2 z + k3` of fresh inputs.
    Row([f64; 4]),
}

/// The fold of one instruction.
enum Folded {
    Keep(Instr),
    Alias(u32),
    Const(f64),
}

/// The canonical emitter (see the module): hash-consing, folding, the bit-exact identities.
pub(crate) struct Emit<'a> {
    alloc: Alloc<'a>,
    most: u32,
    pub instrs: AVec<'a, Instr>,
    pub pool: AVec<'a, f64>,
    pub tables: AVec<'a, Table>,
    pub data: AVec<'a, f64>,
    by_instr: Map<Instr, u32, FixedState, Alloc<'a>>,
    by_group: Map<[u64; 7], u32, FixedState, Alloc<'a>>,
    by_table: Map<(u8, u64), u32, FixedState, Alloc<'a>>,
    /// Whether a fold may have left an emitted instruction unread.
    pub orphans: bool,
}

impl<'a> Emit<'a> {
    pub fn new(alloc: Alloc<'a>, limits: &Limits) -> Emit<'a> {
        let map = || MapOpts { hasher: FixedState, alloc, capacity: None };
        Emit {
            alloc,
            most: limits.most_instrs,
            instrs: Vec::new_in(alloc),
            pool: Vec::new_in(alloc),
            tables: Vec::new_in(alloc),
            data: Vec::new_in(alloc),
            by_instr: Map::new(map()),
            by_group: Map::new(map()),
            by_table: Map::new(map()),
            orphans: false,
        }
    }

    /// The pool index of `values` (at most six), hash-consed by their bits.
    pub fn group(&mut self, values: &[f64]) -> u32 {
        let mut key = [0u64; 7];
        key[0] = values.len() as u64;
        for (k, v) in key[1..].iter_mut().zip(values.iter()) {
            *k = v.to_bits();
        }
        if let Some(i) = self.by_group.get(&key) {
            return *i;
        }
        let i = self.pool.len() as u32;
        self.pool.extend_from_slice(values);
        self.by_group.insert(key, i);
        i
    }

    /// The table index for the source `key` of `kind`, copied in once.
    pub fn table(&mut self, kind: TableKind, key: u64, head: [f64; 8], rows: &[f64]) -> u32 {
        if let Some(i) = self.by_table.get(&(kind as u8, key)) {
            return *i;
        }
        let i = self.tables.len() as u32;
        let start = self.data.len() as u32;
        self.data.extend_from_slice(rows);
        self.tables.push(Table { kind, head, start, len: rows.len() as u32 });
        self.by_table.insert((kind as u8, key), i);
        i
    }

    fn const_of(&self, v: u32) -> Option<f64> {
        let i = &self.instrs[v as usize];
        if i.op == Op::Const { Some(self.pool[i.extra as usize]) } else { None }
    }

    fn is_const(&self, v: u32, x: f64) -> bool {
        self.const_of(v).is_some_and(|c| c.to_bits() == x.to_bits())
    }

    fn fold(&self, i: Instr) -> Folded {
        let op = i.op;
        let [a, b, c, d] = i.args;
        match op {
            Op::Mul if self.is_const(b, 1.0) => return Folded::Alias(a),
            Op::Mul if self.is_const(a, 1.0) => return Folded::Alias(b),
            Op::Div if self.is_const(b, 1.0) => return Folded::Alias(a),
            Op::Sub if self.is_const(b, 0.0) => return Folded::Alias(a),
            Op::Min | Op::Max if a == b => return Folded::Alias(a),
            Op::Select if c == d => return Folded::Alias(c),
            Op::Neg if self.instrs[a as usize].op == Op::Neg => return Folded::Alias(self.instrs[a as usize].args[0]),
            _ => {}
        }
        let pure = matches!(
            op,
            Op::Neg
                | Op::Abs
                | Op::Sqrt
                | Op::Square
                | Op::Sin
                | Op::Cos
                | Op::Round
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Min
                | Op::Max
                | Op::Atan2
                | Op::PowConst
                | Op::Select
        );
        if !pure {
            return Folded::Keep(i);
        }
        if op == Op::Select
            && let (Some(x), Some(y)) = (self.const_of(a), self.const_of(b))
        {
            return Folded::Alias(if x < y { c } else { d });
        }
        let n = op.arity();
        let mut v = [0.0; 4];
        for k in 0..n {
            match self.const_of(i.args[k]) {
                Some(x) => v[k] = x,
                None => return Folded::Keep(i),
            }
        }
        let k = if op == Op::PowConst { self.pool[i.extra as usize] } else { 0.0 };
        Folded::Const(crate::eval::pure::<f64>(op, v[0], v[1], v[2], v[3], k, false))
    }

    /// Emits `i` (folded, hash-consed) and gives its value's index.
    pub fn push(&mut self, i: Instr) -> Result<u32, CompileError> {
        let i = match self.fold(i) {
            Folded::Keep(i) => i,
            Folded::Alias(v) => {
                self.orphans = true;
                return Ok(v);
            }
            Folded::Const(x) => {
                self.orphans = true;
                let extra = self.group(&[x]);
                Instr { op: Op::Const, args: [0; 4], extra }
            }
        };
        if let Some(v) = self.by_instr.get(&i) {
            return Ok(*v);
        }
        if self.instrs.len() as u64 >= u64::from(self.most) {
            return Err(CompileError::TooLong { most: u64::from(self.most) });
        }
        let v = self.instrs.len() as u32;
        self.instrs.push(i);
        self.by_instr.insert(i, v);
        Ok(v)
    }

    /// Emits `op` of `args` with `consts` in the pool.
    pub fn pooled(&mut self, op: Op, args: [u32; 4], consts: &[f64]) -> Result<u32, CompileError> {
        let extra = self.group(consts);
        self.push(Instr { op, args, extra })
    }

    /// The parts, for [`finish`].
    pub fn into_parts(self, root: u32) -> Parts<'a> {
        Parts { instrs: self.instrs, pool: self.pool, tables: self.tables, data: self.data, root }
    }

    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }
}

/// A tape's parts as a source to emit from.
#[derive(Clone, Copy)]
pub(crate) struct Source<'s> {
    pub instrs: &'s [Instr],
    pub pool: &'s [f64],
    pub tables: &'s [Table],
    pub data: &'s [f64],
}

impl<'s> Source<'s> {
    pub fn of(t: &'s Tape<'_>) -> Source<'s> {
        Source { instrs: &t.instrs, pool: &t.pool, tables: &t.tables, data: &t.data }
    }
    pub fn of_emit(e: &'s Emit<'_>) -> Source<'s> {
        Source { instrs: &e.instrs, pool: &e.pool, tables: &e.tables, data: &e.data }
    }
}

const NONE: u32 = u32::MAX;

/// Emits what `root` of `src` reaches into `out`, depth first, operands in order, each source
/// instruction rewritten by `rewrite` (see [`Rw`]); gives the new root. Linear: each source
/// instruction is emitted at most once and pushed once per reader.
pub(crate) fn emit_from(src: Source<'_>, root: u32, out: &mut Emit<'_>, rewrite: impl Fn(u32, &Instr) -> Rw) -> Result<u32, CompileError> {
    let alloc = out.alloc();
    let n = src.instrs.len();
    let mut map: AVec<'_, u32> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        map.push(NONE);
    }
    let mut stack: AVec<'_, (u32, bool)> = Vec::new_in(alloc);
    stack.push((root, false));
    while let Some((i, ready)) = stack.pop() {
        if map[i as usize] != NONE {
            continue;
        }
        let ins = src.instrs[i as usize];
        let rw = rewrite(i, &ins);
        let (deps, nd) = match rw {
            Rw::Keep => (ins.args, ins.op.arity()),
            Rw::Alias(j) => ([j, 0, 0, 0], 1),
            Rw::Row(_) => ([0; 4], 0),
        };
        if !ready {
            stack.push((i, true));
            for k in (0..nd).rev() {
                if map[deps[k] as usize] == NONE {
                    stack.push((deps[k], false));
                }
            }
            continue;
        }
        let m = |v: u32| map[v as usize];
        let v = match rw {
            Rw::Alias(j) => m(j),
            Rw::Row(k) => {
                let x = out.push(Instr { op: Op::X, args: [0; 4], extra: 0 })?;
                let y = out.push(Instr { op: Op::Y, args: [0; 4], extra: 0 })?;
                let z = out.push(Instr { op: Op::Z, args: [0; 4], extra: 0 })?;
                out.pooled(Op::Row, [x, y, z, 0], &k)?
            }
            Rw::Keep => {
                let mut args = [0u32; 4];
                for (a, s) in args.iter_mut().zip(ins.args[..ins.op.arity()].iter()) {
                    *a = m(*s);
                }
                let extra = if ins.op.pool() > 0 {
                    let e = ins.extra as usize;
                    out.group(&src.pool[e..e + ins.op.pool()])
                } else if ins.op.table() {
                    let t = src.tables[ins.extra as usize];
                    let rows = &src.data[t.start as usize..(t.start + t.len) as usize];
                    out.table(t.kind, u64::from(ins.extra), t.head, rows)
                } else {
                    0
                };
                out.push(Instr { op: ins.op, args, extra })?
            }
        };
        map[i as usize] = v;
    }
    Ok(map[root as usize])
}

/// Emits `src` from `root` with `rewrite`, then once more plainly when a fold may have orphaned
/// an instruction, and finishes the tape: the one path every tape is made by.
pub(crate) fn rebuild<'a>(
    src: Source<'_>,
    root: u32,
    rewrite: impl Fn(u32, &Instr) -> Rw,
    level: Level,
    aabb: Aabb3,
    limits: Limits,
    alloc: Alloc<'a>,
    plant: impl Planted<CadPlant>,
) -> Result<Tape<'a>, CompileError> {
    let mut first = Emit::new(alloc, &limits);
    let r1 = emit_from(src, root, &mut first, rewrite)?;
    let (parts, root) = if first.orphans {
        let mut second = Emit::new(alloc, &limits);
        let r2 = emit_from(Source::of_emit(&first), r1, &mut second, |_, _| Rw::Keep)?;
        (second.into_parts(r2), r2)
    } else {
        (first.into_parts(r1), r1)
    };
    let _ = root;
    finish(parts, level, aabb, limits, alloc, plant)
}
