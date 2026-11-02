//! Validity of a value at its type, run on every typed copy: a port of
//! `compiler/rustc_const_eval/src/interpret/validity.rs` (`ValidityVisitor`, non-recursive, with
//! `reset_provenance_and_padding`, as Miri runs it on each typed copy) over hir-ty's types and
//! layouts, and the retag of every reference and `Box` it finds (`check_safe_pointer` calling
//! `M::retag_ptr_value`, which is Tree Borrows' `tb_retag_ptr_value`).
//!
//! Where rustc walks the type at every copy, this compiles each type once into a [`Plan`]: a
//! flat list of [`Op`]s at offsets (structs and tuples flattened, sub-plans only for array
//! elements and enum variants), so a copy runs one loop over a few ops. The verdicts are the
//! visitor's; `tests.rs` checks them case by case against Miri's messages.

use std::{fmt, ops::Range};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::radix;
use hir_def::{
    AdtId, EnumId, EnumVariantId, HasModule, Lookup, VariantId,
    signatures::{StructSignature, VariantFields},
};
use hir_ty::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    drop::is_freeze,
    layout::Layout,
    next_solver::{
        DbInterner, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::implements_trait_unique,
};
use hir_ty_mir::mir::{RetagMode, TypedCx};
use rustc_abi::{BackendRepr, FieldsShape, Primitive, Scalar, TagEncoding, Variants};
use rustc_type_ir::{
    Mutability,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _, SliceLike as _, Ty as _},
};
use smallvec::SmallVec;

use crate::{
    borrows::{self, Borrows, NewPermission, PtrKind},
    ids::{AccessKind, AllocId, Prov, SiteId, Span},
    memory::{Bytes, MemError, Memory},
    race::Races,
    ub::Ub,
};

/// A value invalid at its type, in rustc's words: "constructing invalid value of type
/// `Option<bool>`: at .<enum-variant(Some)>.0, encountered 0x02, but expected a boolean".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueError {
    /// The type of the whole value copied, as rustc spells it.
    pub ty: String,
    /// Where in it the invalid part is (`.0.<enum-variant(Some)>.0`), empty for the whole.
    pub path: String,
    /// What is wrong, as `ValidationErrorKind` and the visitor's `format!`s say it.
    pub msg: String,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // rustc_middle/src/mir/interpret/error.rs, `ValidationError`.
        if self.path.is_empty() {
            write!(f, "constructing invalid value of type {}: {}", self.ty, self.msg)
        } else {
            write!(f, "constructing invalid value of type {}: at {}, {}", self.ty, self.path, self.msg)
        }
    }
}

/// Why a typed copy stopped the run.
#[derive(Debug)]
pub(crate) enum Fail {
    /// Undefined behavior: an invalid value, or what memory, Tree Borrows or the race detector
    /// refused while retagging.
    Ub(Ub),
    /// The checker can't check this value: it names what (a layout hir-ty can't compute, an
    /// array of no evaluated length, a coroutine).
    Unsupported(String),
}

impl From<Ub> for Fail {
    fn from(it: Ub) -> Fail {
        Fail::Ub(it)
    }
}

type R<T = ()> = Result<T, Fail>;

/// What a validity message says was expected (validity.rs `ExpectedKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
    Reference,
    Box,
    RawPtr,
    Bool,
    Char,
    Float,
    Int,
    FnPtr,
}

impl Expected {
    fn as_str(self) -> &'static str {
        match self {
            Expected::Reference => "expected a reference",
            Expected::Box => "expected a box",
            Expected::RawPtr => "expected a raw pointer",
            Expected::Bool => "expected a boolean",
            Expected::Char => "expected a unicode scalar value",
            Expected::Float => "expected a floating point number",
            Expected::Int => "expected an integer",
            Expected::FnPtr => "expected a function pointer",
        }
    }
}

/// A reference or a `Box` (validity.rs `PtrKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SafePtr {
    Ref(Mutability),
    Box { global: bool },
}

impl SafePtr {
    fn noun(self) -> &'static str {
        match self {
            SafePtr::Ref(_) => "reference",
            SafePtr::Box { .. } => "box",
        }
    }

    fn expected(self) -> Expected {
        match self {
            SafePtr::Ref(_) => Expected::Reference,
            SafePtr::Box { .. } => Expected::Box,
        }
    }
}

/// One step of a path into a value (validity.rs `PathElem`), resolved to names only when an
/// error is rendered.
#[derive(Clone, Copy, Debug)]
enum Elem {
    Root,
    Field(VariantId, u32),
    Tuple(u32),
    Captured(u32),
    EnumTag,
}

#[derive(Clone, Copy, Debug)]
struct PathNode {
    parent: u32,
    elem: Elem,
}

/// How a sub-plan was entered at run time, for the path of an error.
#[derive(Clone, Copy, Debug)]
enum Entered {
    Index(u64),
    Variant(EnumVariantId),
}

/// The unsized tail of a pointee (rustc's `struct_tail`), which says what the metadata is.
#[derive(Clone, Copy, Debug)]
enum Tail {
    Slice { elem_size: u64, elem_align: u64, elem_cells: (u32, u32) },
    Str,
    Dyn,
    Foreign,
}

/// What a reference, `Box` or wide raw pointer needs of its pointee type, computed once.
#[derive(Clone, Copy, Debug)]
struct Pointee<'db> {
    ty: Ty<'db>,
    freeze: bool,
    unpin: bool,
    inhabited: bool,
    shape: Shape,
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    Sized { size: u64, align: u64, cells: (u32, u32) },
    /// A type whose size comes from metadata: the chain of struct levels down to the tail
    /// (offset of the tail field and alignment of the sized part, outermost first), the
    /// `UnsafeCell` ranges of the sized prefix, and the tail.
    Unsized { levels: (u32, u32), cells: (u32, u32), tail: Tail },
}

#[derive(Clone, Copy, Debug)]
enum Op<'db> {
    Bool { at: u64, path: u32 },
    Char { at: u64, path: u32 },
    Int { at: u64, size: u8, float: bool, path: u32 },
    /// A run of integers or floats (validity.rs' array fast path): one init check, one reset.
    Scalars { at: u64, len: u64, elem: u64, float: bool, path: u32 },
    RawPtr { at: u64, tail: Option<u32>, path: u32 },
    FnPtr { at: u64, path: u32 },
    Safe { at: u64, kind: SafePtr, pointee: u32, wide: bool, path: u32 },
    Never { path: u32 },
    Variantless { ty: Ty<'db>, path: u32 },
    Union { at: u64, data: (u32, u32) },
    /// A scalar's valid range from the layout (`ty::Pat`, `NonZero`, `NonNull`), checked after
    /// its fields as `visit_scalar`.
    Range { at: u64, size: u8, start: u128, end: u128, path: u32 },
    Array { at: u64, count: u64, stride: u64, elem: u32, path: u32 },
    Enum { at: u64, plan: u32, path: u32, tag_path: u32 },
}

/// An enum's tag and variants (rustc `read_discriminant`).
#[derive(Clone, Copy, Debug)]
struct EnumPlan {
    tag_at: u64,
    tag_size: u8,
    tag_ptr: bool,
    valid_start: u128,
    valid_end: u128,
    encoding: Encoding,
    variants: (u32, u32),
}

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Direct,
    Niche { untagged: u32, start: u32, last: u32, niche_start: u128 },
}

#[derive(Clone, Copy, Debug)]
struct VariantPlan {
    id: EnumVariantId,
    /// The discriminant, truncated to the tag's size.
    discr: u128,
    inhabited: bool,
    plan: u32,
}

#[derive(Clone, Copy, Debug)]
struct Plan {
    ops: (u32, u32),
    size: u64,
    /// Every byte is data and no enum or union decides which: nothing to reset as padding.
    dense: bool,
}

fn range(r: (u32, u32)) -> Range<usize> {
    r.0 as usize..r.1 as usize
}

/// Every plan and pointee the run has needed, by type: a column each, so a hit is one hash
/// lookup and nothing allocated.
pub(crate) struct Plans<'db> {
    plans: Vec<Plan>,
    ops: Vec<Op<'db>>,
    by_ty: HashMap<Ty<'db>, u32>,
    enums: Vec<EnumPlan>,
    variants: Vec<VariantPlan>,
    paths: Vec<PathNode>,
    pointees: Vec<Pointee<'db>>,
    pointee_by_ty: HashMap<Ty<'db>, u32>,
    cells: Vec<Span>,
    levels: Vec<(u64, u64)>,
    unions: Vec<(u64, u64)>,
    layouts: HashMap<Ty<'db>, &'static Layout>,
    infcx: Option<(base_db::Crate, InferCtxt<'db>)>,
    // Scratch, reused by every copy.
    data: Vec<(u64, u64)>,
    entered: Vec<(u32, Entered)>,
    cell_buf: Vec<Span>,
    order: Vec<u32>,
}

impl<'db> Default for Plans<'db> {
    fn default() -> Self {
        Plans {
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            plans: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            ops: Vec::new(),
            by_ty: HashMap::default(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            enums: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            variants: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            paths: vec![PathNode { parent: 0, elem: Elem::Root }],
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            pointees: Vec::new(),
            pointee_by_ty: HashMap::default(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            cells: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            levels: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            unions: Vec::new(),
            layouts: HashMap::default(),
            infcx: None,
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            data: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            entered: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            cell_buf: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            order: Vec::new(),
        }
    }
}

/// The checker's parts a typed copy touches.
pub(crate) struct Parts<'c> {
    pub mem: &'c mut Memory,
    pub borrows: &'c mut Borrows,
    pub races: &'c mut Races,
    pub site: SiteId,
}

/// `Machine::typed`: checks the value at `place` (`bytes`, already written) at `ty`, retags
/// every reference and `Box` in it by `mode`, and resets its padding and the provenance of
/// its non-pointer bytes.
pub(crate) fn typed<'db>(
    plans: &mut Plans<'db>,
    cx: &TypedCx<'_, 'db>,
    parts: Parts<'_>,
    loc: Option<(AllocId, u64)>,
    ty: Ty<'db>,
    bytes: &[u8],
    mode: RetagMode,
) -> R {
    let plan = plans.plan(cx, ty)?;
    let shadow = match loc {
        Some((alloc, at)) => parts.mem.read(alloc, Span::new(at, bytes.len() as u64)),
        None => Bytes::init(0),
    };
    let ptr = cx.target.pointer_size().bytes();
    let mut run = Run {
        plans,
        cx,
        parts,
        loc,
        bytes,
        shadow: &shadow,
        has_prov: shadow.has_prov(0..shadow.len()),
        mode,
        ptr,
        ty,
    };
    run.plans.data.clear();
    run.plans.entered.clear();
    run.run(plan, 0)?;
    run.reset_padding(plan)
}

struct Run<'r, 'c, 'a, 'db> {
    plans: &'r mut Plans<'db>,
    cx: &'r TypedCx<'a, 'db>,
    parts: Parts<'c>,
    /// The allocation and offset of the value (`None`: it has no bytes).
    loc: Option<(AllocId, u64)>,
    bytes: &'r [u8],
    shadow: &'r Bytes,
    has_prov: bool,
    mode: RetagMode,
    ptr: u64,
    ty: Ty<'db>,
}

impl<'db> Run<'_, '_, '_, 'db> {
    fn run(&mut self, plan: u32, base: u64) -> R {
        let ops = range(self.plans.plans[plan as usize].ops);
        for i in ops {
            let op = self.plans.ops[i];
            self.op(op, base)?;
        }
        Ok(())
    }

    fn op(&mut self, op: Op<'db>, base: u64) -> R {
        match op {
            Op::Bool { at, path } => {
                let at = base + at;
                let v = self.scalar(at, 1, Expected::Bool, path)?;
                if v > 1 {
                    // alloc: error path: the message of a failure.
                    return Err(self.invalid(path, format!("encountered {}, but expected a boolean", hex(v, 1))));
                }
                self.data(at, 1, true);
            }
            Op::Char { at, path } => {
                let at = base + at;
                let v = self.scalar(at, 4, Expected::Char, path)?;
                if char::from_u32(v as u32).is_none() {
                    return Err(self.invalid(
                        path,
                        // alloc: error path: the message of a failure.
                        format!(
                            "encountered {}, but expected a valid unicode scalar value (in `0..=0x10FFFF` but not in `0xD800..=0xDFFF`)",
                            hex(v, 4)
                        ),
                    ));
                }
                self.data(at, 4, true);
            }
            Op::Int { at, size, float, path } => {
                let at = base + at;
                let expected = if float { Expected::Float } else { Expected::Int };
                self.require_init(at, size as u64, expected, path)?;
                self.data(at, size as u64, true);
            }
            Op::Scalars { at, len, elem, float, path } => {
                let at = base + at;
                let expected = if float { Expected::Float } else { Expected::Int };
                if let Err(bad) = self.first_uninit(at, len) {
                    let index = (bad - at) / elem;
                    self.plans.entered.push((path, Entered::Index(index)));
                    // alloc: error path: the message of a failure.
                    return Err(self.invalid(0, format!("encountered uninitialized memory, but {}", expected.as_str())));
                }
                self.data(at, len, true);
            }
            Op::RawPtr { at, tail, path } => {
                let at = base + at;
                self.require_init(at, self.ptr, Expected::RawPtr, path)?;
                // A thin pointer keeps its provenance only if it has a whole one
                // (`reset_pointer_provenance`).
                let keep = self.shadow.prov_at(at as usize, self.ptr as usize).is_some();
                self.data(at, self.ptr, !keep);
                if let Some(tail) = tail {
                    self.require_init(at + self.ptr, self.ptr, Expected::RawPtr, path)?;
                    self.check_meta(at, self.plans.pointees[tail as usize].shape, path, "raw pointer")?;
                    self.data(at + self.ptr, self.ptr, true);
                }
            }
            Op::FnPtr { at, path } => {
                let at = base + at;
                let v = self.scalar(at, self.ptr, Expected::FnPtr, path)?;
                if v == 0 {
                    // alloc: error path: the message of a failure.
                    return Err(self.invalid(path, "encountered a null function pointer".to_owned()));
                }
                let keep = self.shadow.prov_at(at as usize, self.ptr as usize).is_some();
                self.data(at, self.ptr, !keep);
            }
            Op::Safe { at, kind, pointee, path, .. } => self.safe_pointer(base + at, kind, pointee, path)?,
            Op::Never { path } => {
                // alloc: error path: the message of a failure.
                return Err(self.invalid(path, "encountered a value of the never type `!`".to_owned()));
            }
            Op::Variantless { ty, path } => {
                let ty = type_name(self.cx.db, ty);
                // alloc: error path: the message of a failure.
                return Err(self.invalid(path, format!("encountered a value of zero-variant enum `{ty}`")));
            }
            Op::Union { at, data } => {
                for i in range(data) {
                    let (off, len) = self.plans.unions[i];
                    self.data(base + at + off, len, false);
                }
            }
            Op::Range { at, size, start, end, path } => self.valid_range(base + at, size, start, end, path)?,
            Op::Array { at, count, stride, elem, path } => {
                for i in 0..count {
                    self.plans.entered.push((path, Entered::Index(i)));
                    self.run(elem, base + at + i * stride)?;
                    self.plans.entered.pop();
                }
            }
            Op::Enum { at, plan, path, tag_path } => self.enum_value(base + at, plan, path, tag_path)?,
        }
        Ok(())
    }

    /// The little-endian integer of `size` bytes at `at`, which must be initialized.
    fn scalar(&mut self, at: u64, size: u64, expected: Expected, path: u32) -> R<u128> {
        self.require_init(at, size, expected, path)?;
        Ok(le(&self.bytes[at as usize..(at + size) as usize]))
    }

    fn require_init(&mut self, at: u64, size: u64, expected: Expected, path: u32) -> R {
        if self.shadow.require_init(at as usize..(at + size) as usize).is_err() {
            // alloc: error path: the message of a failure.
            return Err(self.invalid(path, format!("encountered uninitialized memory, but {}", expected.as_str())));
        }
        Ok(())
    }

    /// The first uninitialized byte of `at..at + len`, found by halving.
    fn first_uninit(&self, at: u64, len: u64) -> Result<(), u64> {
        let (mut lo, mut hi) = (at as usize, (at + len) as usize);
        if self.shadow.require_init(lo..hi).is_ok() {
            return Ok(());
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.shadow.require_init(lo..mid).is_err() {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Err(lo as u64)
    }

    /// Marks `at..at + len` as data (not padding), and drops its provenance if `clear`.
    fn data(&mut self, at: u64, len: u64, clear: bool) {
        if len == 0 {
            return;
        }
        self.plans.data.push((at, len));
        if clear && self.has_prov
            && let Some((alloc, base)) = self.loc
        {
            self.parts.mem.clear_prov(alloc, Span::new(base + at, len));
        }
    }

    /// The scalar range check of `visit_scalar`.
    fn valid_range(&mut self, at: u64, size: u8, start: u128, end: u128, path: u32) -> R {
        let max = u128::MAX >> (128 - 8 * size as u32);
        let contains = |v: u128| if start <= end { start <= v && v <= end } else { v >= start || v <= end };
        if self.shadow.prov_at(at as usize, size as usize).is_some() {
            // A pointer: only "not null" can be decided of it.
            let addr = le(&self.bytes[at as usize..(at + size as u64) as usize]);
            if start == 1 && end == max {
                if addr == 0 {
                    return Err(self.invalid(
                        path,
                        // alloc: error path: the message of a failure.
                        "encountered a maybe-null pointer, but expected something that is definitely non-zero".to_owned(),
                    ));
                }
                return Ok(());
            }
            if end.wrapping_add(1) & max == start {
                return Ok(());
            }
            return Err(self.invalid(
                path,
                // alloc: error path: the message of a failure.
                format!(
                    "encountered a pointer with unknown absolute address, but expected something that is definitely {}",
                    fmt_range(start, end, max)
                ),
            ));
        }
        let bits = le(&self.bytes[at as usize..(at + size as u64) as usize]);
        if contains(bits) {
            Ok(())
        } else {
            // alloc: error path: the message of a failure.
            Err(self.invalid(path, format!("encountered {bits}, but expected something {}", fmt_range(start, end, max))))
        }
    }

    /// The size and alignment the pointer at `at` points to, from its metadata, checking the
    /// metadata (`check_wide_ptr_meta`). `None` for a sized pointee.
    fn check_meta(&mut self, at: u64, shape: Shape, path: u32, noun: &str) -> R<Option<(u64, u64, Option<Ty<'db>>)>> {
        let Shape::Unsized { levels, tail, .. } = shape else { return Ok(None) };
        let meta = le(&self.bytes[(at + self.ptr) as usize..(at + 2 * self.ptr) as usize]);
        let (size, align, concrete) = match tail {
            Tail::Slice { elem_size, elem_align, .. } => {
                let Some(size) = (meta as u64).checked_mul(elem_size) else {
                    return Err(self.invalid(
                        path,
                        // alloc: error path: the message of a failure.
                        format!("encountered invalid {noun} metadata: slice is bigger than largest supported object"),
                    ));
                };
                (size, elem_align, None)
            }
            Tail::Str => (meta as u64, 1, None),
            Tail::Foreign => (0, 1, None),
            Tail::Dyn => {
                let Ok(concrete) = self.cx.vtables.ty(meta as usize) else {
                    return Err(self.invalid(
                        path,
                        // alloc: error path: the message of a failure.
                        format!("encountered {:#x}[noalloc], but expected a vtable pointer", meta),
                    ));
                };
                let (size, align) = self.plans.size_align(self.cx, concrete)?;
                (size, align, Some(concrete))
            }
        };
        // rustc `size_and_align_from_meta`: each struct level from the tail out.
        let (mut size, mut align) = (size, align);
        for i in range(levels).rev() {
            let (offset, sized_align) = self.plans.levels[i];
            align = align.max(sized_align);
            size = (offset + size).next_multiple_of(align);
        }
        let limit = 1u64 << (8 * self.ptr - 1);
        if size >= limit {
            return Err(self.invalid(
                path,
                // alloc: error path: the message of a failure.
                format!("encountered invalid {noun} metadata: total size is bigger than largest supported object"),
            ));
        }
        Ok(Some((size, align, concrete)))
    }

    /// `check_safe_pointer`, with the retag of `tb_retag_ptr_value`.
    fn safe_pointer(&mut self, at: u64, kind: SafePtr, pointee: u32, path: u32) -> R {
        let noun = kind.noun();
        let addr = self.scalar(at, self.ptr, kind.expected(), path)? as u64;
        let prov = self.shadow.prov_at(at as usize, self.ptr as usize);
        let info = self.plans.pointees[pointee as usize];
        let wide = matches!(info.shape, Shape::Unsized { .. });
        if wide {
            self.require_init(at + self.ptr, self.ptr, kind.expected(), path)?;
        }
        let (size, align, concrete) = match self.check_meta(at, info.shape, path, noun)? {
            Some(it) => it,
            None => match info.shape {
                Shape::Sized { size, align, .. } => (size, align, None),
                Shape::Unsized { .. } => unreachable!("an unsized pointee has metadata"),
            },
        };
        let target = match self.parts.mem.access(addr, prov, size, 1, AccessKind::Read) {
            Ok(it) => it,
            Err(e) => return Err(self.deref_error(e, noun, addr, path)),
        };
        // The pointer keeps a whole provenance, possibly retagged; anything else is dropped.
        let mut new_prov = prov;
        if let (Some((palloc, poff)), Some(parent)) = (target, prov) {
            let ptr_kind = match kind {
                SafePtr::Ref(Mutability::Not) => Some(PtrKind::Shared),
                SafePtr::Ref(Mutability::Mut) => Some(PtrKind::Mut),
                SafePtr::Box { global: true } => Some(PtrKind::Box),
                // tb_retag_ptr_value: no retag of a `Box` of another allocator.
                SafePtr::Box { global: false } => None,
            };
            if let Some(ptr_kind) = ptr_kind {
                new_prov = self.retag(ptr_kind, &info, concrete, size, (palloc, poff), parent)?.or(prov);
            }
        }
        match (new_prov, self.loc) {
            (Some(p), Some((alloc, base))) if new_prov != prov => {
                self.parts.mem.set_prov(alloc, base + at, self.ptr, p)
            }
            _ => {}
        }
        self.data(at, self.ptr, prov.is_none());
        if wide {
            self.data(at + self.ptr, self.ptr, true);
        }
        // Alignment after dereferenceability, as rustc.
        if addr != 0 && addr % align != 0 {
            let has = 1u64 << addr.trailing_zeros().min(29);
            return Err(self.invalid(
                path,
                // alloc: error path: the message of a failure.
                format!("encountered an unaligned {noun} (required {align} byte alignment but found {has})"),
            ));
        }
        if addr == 0 {
            // alloc: error path: the message of a failure.
            return Err(self.invalid(path, format!("encountered a null {noun}")));
        }
        if !info.inhabited {
            let ty = type_name(self.cx.db, info.ty);
            // alloc: error path: the message of a failure.
            return Err(self.invalid(path, format!("encountered a {noun} pointing to uninhabited type `{ty}`")));
        }
        Ok(())
    }

    /// The validity message for a pointer `check_ptr_access` refused.
    fn deref_error(&mut self, e: MemError, noun: &str, addr: u64, path: u32) -> Fail {
        let msg = match &e {
            // alloc: error path: the message of a failure.
            MemError::DanglingInt { addr: 0, .. } => format!("encountered a null {noun}"),
            MemError::DanglingInt { .. } => {
                // alloc: error path: the message of a failure.
                format!("encountered a dangling {noun} ({addr:#x}[noalloc] has no provenance)")
            }
            MemError::OutOfBounds { .. } => {
                // alloc: error path: the message of a failure.
                format!("encountered a dangling {noun} (going beyond the bounds of its allocation)")
            }
            // alloc: error path: the message of a failure.
            MemError::UseAfterFree { .. } => format!("encountered a dangling {noun} (use-after-free)"),
            _ => return Fail::Ub(Ub::Memory(e)),
        };
        self.invalid(path, msg)
    }

    /// Tree Borrows' retag of a reference or `Box` to `size` bytes at `target`, from `parent`:
    /// its new provenance (`None`: the retag is a no-op and the pointer keeps its own).
    fn retag(
        &mut self,
        kind: PtrKind,
        info: &Pointee<'db>,
        concrete: Option<Ty<'db>>,
        size: u64,
        (palloc, poff): (AllocId, u64),
        parent: Prov,
    ) -> R<Option<Prov>> {
        let mode = match self.mode {
            RetagMode::Default => borrows::RetagMode::Default,
            RetagMode::TwoPhase => borrows::RetagMode::TwoPhase,
            RetagMode::FnEntry => borrows::RetagMode::FnEntry,
            RetagMode::Raw => borrows::RetagMode::Raw,
            RetagMode::None => borrows::RetagMode::None,
        };
        self.cells(info, concrete, size)?;
        let cells = std::mem::take(&mut self.plans.cell_buf);
        let perm = NewPermission::new(kind, mode, info.freeze, info.unpin, &cells);
        let site = self.parts.site;
        let retagged = self.parts.borrows.retag(palloc, parent, perm, Span::new(poff, size), site);
        let result = match retagged {
            Ok(Some(tag)) => {
                // tb_reborrow: the race detector sees the retag's implicit accesses.
                let Parts { borrows, races, .. } = &mut self.parts;
                let mut race = Ok(());
                for &(span, access) in borrows.retag_accesses() {
                    race = races.retag(palloc, span, access, site);
                    if race.is_err() {
                        break;
                    }
                }
                match race {
                    Ok(()) => Ok(Some(Prov::Concrete { alloc: palloc, tag })),
                    Err(e) => Err(Fail::Ub(e.into())),
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Fail::Ub(e.into())),
        };
        self.plans.cell_buf = cells;
        result
    }

    /// The `UnsafeCell` ranges of the pointee into `cell_buf`, relative to its start, sorted
    /// and merged (Miri `visit_freeze_sensitive`).
    fn cells(&mut self, info: &Pointee<'db>, concrete: Option<Ty<'db>>, size: u64) -> R {
        let plans = &mut *self.plans;
        plans.cell_buf.clear();
        if info.freeze {
            return Ok(());
        }
        match info.shape {
            Shape::Sized { cells, .. } => plans.cell_buf.extend_from_slice(&plans.cells[range(cells)]),
            Shape::Unsized { levels, cells, tail } => {
                plans.cell_buf.extend_from_slice(&plans.cells[range(cells)]);
                let tail_at: u64 = plans.levels[range(levels)].iter().map(|it| it.0).sum();
                match tail {
                    Tail::Slice { elem_size, elem_cells, .. } if elem_cells.1 > elem_cells.0 && elem_size > 0 => {
                        let mut at = tail_at;
                        while at + elem_size <= size {
                            for i in range(elem_cells) {
                                let c = plans.cells[i];
                                plans.cell_buf.push(Span::new(at + c.start, c.len));
                            }
                            at += elem_size;
                        }
                    }
                    Tail::Dyn => {
                        if let Some(concrete) = concrete {
                            let p = plans.pointee(self.cx, concrete)?;
                            if let Shape::Sized { cells, .. } = plans.pointees[p as usize].shape {
                                for i in range(cells) {
                                    let c = plans.cells[i];
                                    plans.cell_buf.push(Span::new(tail_at + c.start, c.len));
                                }
                            }
                        }
                    }
                    Tail::Slice { .. } | Tail::Str | Tail::Foreign => {}
                }
            }
        }
        merge(&mut plans.cell_buf);
        Ok(())
    }

    /// rustc `read_discriminant`, then the variant's fields.
    fn enum_value(&mut self, at: u64, plan: u32, path: u32, tag_path: u32) -> R {
        let e = self.plans.enums[plan as usize];
        let tag_at = at + e.tag_at;
        let size = e.tag_size as u64;
        let expected = if e.tag_ptr { Expected::RawPtr } else { Expected::Int };
        let bits = self.scalar(tag_at, size, expected, tag_path)?;
        let keep = e.tag_ptr && self.shadow.prov_at(tag_at as usize, size as usize).is_some();
        self.data(tag_at, size, !keep);
        let variants = range(e.variants);
        let invalid_tag = |this: &mut Self| {
            // alloc: error path: the message of a failure.
            this.invalid(tag_path, format!("encountered {}, but expected a valid enum tag", hex(bits, size)))
        };
        let contains = |v: u128| {
            if e.valid_start <= e.valid_end {
                e.valid_start <= v && v <= e.valid_end
            } else {
                v >= e.valid_start || v <= e.valid_end
            }
        };
        let index = match e.encoding {
            Encoding::Direct => {
                if !contains(bits) {
                    return Err(invalid_tag(self));
                }
                match self.plans.variants[range(e.variants)].iter().position(|v| v.discr == bits) {
                    Some(i) => i as u32,
                    None => return Err(invalid_tag(self)),
                }
            }
            Encoding::Niche { untagged, start, last, niche_start } => {
                if keep {
                    // A pointer with provenance: only the null niche can be told apart.
                    if !(niche_start == 0 && start == last && bits != 0) {
                        return Err(invalid_tag(self));
                    }
                    untagged
                } else {
                    let max = u128::MAX >> (128 - 8 * size as u32);
                    let relative = bits.wrapping_sub(niche_start) & max;
                    if relative <= (last - start) as u128 {
                        let index = start + relative as u32;
                        if index == untagged {
                            return Err(invalid_tag(self));
                        }
                        index
                    } else {
                        if !contains(bits) {
                            return Err(invalid_tag(self));
                        }
                        untagged
                    }
                }
            }
        };
        let variant = self.plans.variants[variants.start + index as usize];
        if !variant.inhabited {
            // alloc: error path: the message of a failure.
            return Err(self.invalid(tag_path, "encountered an uninhabited enum variant".to_owned()));
        }
        self.plans.entered.push((path, Entered::Variant(variant.id)));
        self.run(variant.plan, at)?;
        self.plans.entered.pop();
        Ok(())
    }

    /// Makes the bytes of the value no field covers uninitialized (`reset_padding`).
    fn reset_padding(&mut self, plan: u32) -> R {
        let Some((alloc, base)) = self.loc else { return Ok(()) };
        let Plan { size, dense, .. } = self.plans.plans[plan as usize];
        if dense {
            return Ok(());
        }
        let data = &mut self.plans.data;
        radix::sort_copy_by_key(data, |&(at, len)| radix::pack_64(at, len));
        let mut done = 0;
        for &(at, len) in data.iter() {
            if at > done {
                self.parts.mem.uninit(alloc, Span::new(base + done, at - done));
            }
            done = done.max(at + len);
        }
        if size > done {
            self.parts.mem.uninit(alloc, Span::new(base + done, size - done));
        }
        Ok(())
    }

    /// A validity error at `path` (in the innermost sub-plan entered), with the path from the
    /// value's root rendered as rustc's `write_path`.
    fn invalid(&mut self, path: u32, msg: String) -> Fail {
        // alloc: error path: the message of a failure.
        let mut out = String::new();
        let entered = std::mem::take(&mut self.plans.entered);
        for &(p, how) in &entered {
            self.plans.write_path(self.cx.db, &mut out, p);
            match how {
                // alloc: error path: the message of a failure.
                Entered::Index(i) => out.push_str(&format!("[{i}]")),
                Entered::Variant(v) => {
                    let name = variant_name(self.cx.db, v);
                    // alloc: error path: the message of a failure.
                    out.push_str(&format!(".<enum-variant({name})>"));
                }
            }
        }
        self.plans.write_path(self.cx.db, &mut out, path);
        self.plans.entered = entered;
        Fail::Ub(Ub::Value(ValueError { ty: type_name(self.cx.db, self.ty), path: out, msg }))
    }
}

impl<'db> Plans<'db> {
    /// For building a plan, once per type.
    fn layout(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>) -> R<&'static Layout> {
        if let Some(&it) = self.layouts.get(&ty) {
            return Ok(it);
        }
        let layout = cx.db.layout_of_ty(ty.store(), cx.env.store()).map_err(|e| {
            // alloc: error path.
            Fail::Unsupported(format!("the layout of `{}`: {e:?}", type_name(cx.db, ty)))
        })?;
        let layout = &layout.get().0;
        self.layouts.insert(ty, layout);
        Ok(layout)
    }

    /// The size and alignment of `ty`, read from the cached layout without cloning it: a
    /// `dyn` pointee's concrete type is looked up on every typed copy of the pointer.
    fn size_align(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>) -> R<(u64, u64)> {
        if let Some(it) = self.layouts.get(&ty) {
            return Ok((it.size.bytes(), it.align.abi.bytes()));
        }
        let layout = self.layout(cx, ty)?;
        Ok((layout.size.bytes(), layout.align.abi.bytes()))
    }

    fn infcx(&mut self, env: ParamEnvAndCrate<'db>, db: &'db Db) -> &InferCtxt<'db> {
        if self.infcx.as_ref().is_none_or(|(krate, _)| *krate != env.krate) {
            let infcx = DbInterner::new_with(db, env.krate).infer_ctxt().build(TypingMode::PostAnalysis);
            self.infcx = Some((env.krate, infcx));
        }
        &self.infcx.as_ref().expect("built above").1
    }

    fn path(&mut self, parent: u32, elem: Elem) -> u32 {
        self.paths.push(PathNode { parent, elem });
        (self.paths.len() - 1) as u32
    }

    fn write_path(&self, db: &Db, out: &mut String, mut path: u32) {
        let mut elems: SmallVec<[Elem; 8]> = SmallVec::new();
        while path != 0 {
            let node = self.paths[path as usize];
            elems.push(node.elem);
            path = node.parent;
        }
        for elem in elems.iter().rev() {
            match *elem {
                Elem::Root => {}
                Elem::Field(variant, i) => {
                    let fields = VariantFields::of(db, variant);
                    match fields.fields().iter().nth(i as usize) {
                        // alloc: error path: the message of a failure.
                        Some((_, field)) => out.push_str(&format!(".{}", field.name.as_str())),
                        // alloc: error path: the message of a failure.
                        None => out.push_str(&format!(".{i}")),
                    }
                }
                // alloc: error path: the message of a failure.
                Elem::Tuple(i) => out.push_str(&format!(".{i}")),
                // alloc: error path: the message of a failure.
                Elem::Captured(i) => out.push_str(&format!(".<captured-var({i})>")),
                Elem::EnumTag => out.push_str(".<enum-tag>"),
            }
        }
    }

    /// The plan of `ty`, compiled on first use.
    fn plan(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>) -> R<u32> {
        if let Some(&it) = self.by_ty.get(&ty) {
            return Ok(it);
        }
        let layout = self.layout(cx, ty)?;
        // alloc: plan building, once per type.
        let mut ops = Vec::new();
        let mut dense = true;
        self.compile(cx, ty, &layout, 0, 0, &mut ops, &mut dense)?;
        // Padding: bytes not covered by any op's data.
        if dense {
            // alloc: plan building, once per type.
            let mut covered: Vec<(u64, u64)> = Vec::new();
            for op in &ops {
                op_data(op, cx.target.pointer_size().bytes(), &mut covered);
            }
            radix::sort_copy_by_key(&mut covered, |&(at, len)| radix::pack_64(at, len));
            let mut done = 0;
            for (at, len) in covered {
                if at > done {
                    dense = false;
                    break;
                }
                done = done.max(at + len);
            }
            dense &= done >= layout.size.bytes();
        }
        let start = self.ops.len() as u32;
        self.ops.extend(ops);
        let id = self.plans.len() as u32;
        self.plans.push(Plan { ops: (start, self.ops.len() as u32), size: layout.size.bytes(), dense });
        self.by_ty.insert(ty, id);
        Ok(id)
    }

    /// The ops of a value of `ty` at `at` into `ops`, flattening structs and tuples (validity.rs
    /// `visit_value`, `try_visit_primitive`, `walk_value`).
    #[allow(clippy::too_many_arguments)]
    fn compile(
        &mut self,
        cx: &TypedCx<'_, 'db>,
        ty: Ty<'db>,
        layout: &Layout,
        at: u64,
        path: u32,
        ops: &mut Vec<Op<'db>>,
        dense: &mut bool,
    ) -> R {
        let db = cx.db;
        let interner = DbInterner::new_with(db, cx.env.krate);
        match ty.kind() {
            TyKind::Bool => ops.push(Op::Bool { at, path }),
            TyKind::Char => ops.push(Op::Char { at, path }),
            TyKind::Int(_) | TyKind::Uint(_) => {
                ops.push(Op::Int { at, size: layout.size.bytes() as u8, float: false, path })
            }
            TyKind::Float(_) => ops.push(Op::Int { at, size: layout.size.bytes() as u8, float: true, path }),
            TyKind::RawPtr(pointee, _) => {
                let p = self.pointee(cx, pointee)?;
                let tail = matches!(self.pointees[p as usize].shape, Shape::Unsized { .. }).then_some(p);
                ops.push(Op::RawPtr { at, tail, path });
            }
            TyKind::Ref(_, pointee, mutbl) => {
                let pointee = self.pointee(cx, pointee)?;
                let wide = matches!(self.pointees[pointee as usize].shape, Shape::Unsized { .. });
                ops.push(Op::Safe { at, kind: SafePtr::Ref(mutbl), pointee, wide, path });
            }
            TyKind::FnPtr(..) => ops.push(Op::FnPtr { at, path }),
            TyKind::Never => ops.push(Op::Never { path }),
            TyKind::FnDef(..) | TyKind::Foreign(_) => {}
            TyKind::Pat(base, _) => {
                let base_layout = self.layout(cx, base)?;
                self.compile(cx, base, &base_layout, at, path, ops, dense)?;
                self.scalar_ranges(cx, layout, at, path, ops);
            }
            TyKind::Tuple(fields) => {
                for (i, field) in fields.iter().enumerate() {
                    let fpath = self.path(path, Elem::Tuple(i as u32));
                    let off = layout.fields.offset(i).bytes();
                    let fl = self.layout(cx, field)?;
                    self.compile(cx, field, &fl, at + off, fpath, ops, dense)?;
                }
            }
            TyKind::Closure(_, args) => {
                let upvars = args.as_closure().tupled_upvars_ty();
                for (i, field) in upvars.tuple_fields().iter().enumerate() {
                    let fpath = self.path(path, Elem::Captured(i as u32));
                    let off = layout.fields.offset(i).bytes();
                    let fl = self.layout(cx, field)?;
                    self.compile(cx, field, &fl, at + off, fpath, ops, dense)?;
                }
            }
            TyKind::Array(elem, len) => {
                let Some(count) = try_const_usize(db, len) else {
                    // alloc: plan building, once per type (unsupported: an error).
                    return Err(Fail::Unsupported(format!(
                        "an array of no evaluated length, `{}`",
                        type_name(db, ty)
                    )));
                };
                let count = count as u64;
                let el = self.layout(cx, elem)?;
                let stride = el.size.bytes();
                if count == 0 {
                } else if matches!(elem.kind(), TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)) {
                    let float = matches!(elem.kind(), TyKind::Float(_));
                    ops.push(Op::Scalars { at, len: stride * count, elem: stride, float, path });
                } else if stride == 0 {
                    // validity.rs: one element of an array of ZSTs stands for all.
                    let elem_plan = self.plan(cx, elem)?;
                    ops.push(Op::Array { at, count: 1, stride: 0, elem: elem_plan, path });
                } else {
                    let elem_plan = self.plan(cx, elem)?;
                    *dense &= self.plans[elem_plan as usize].dense;
                    ops.push(Op::Array { at, count, stride, elem: elem_plan, path });
                }
            }
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) if def.is_box() => {
                    let global = box_is_global(db, args);
                    let pointee = self.pointee(cx, args.type_at(0))?;
                    // walk_value's `Box`: the raw pointer at the end of field 0's chain (`Unique`,
                    // `NonNull`, the pattern type), then the allocator, field 1, as a field.
                    let fields = db.field_types(id.into());
                    let mut ptr_at = at + layout.fields.offset(0).bytes();
                    let mut inner = fields
                        .iter()
                        .next()
                        .map(|(_, f)| f.ty().instantiate(interner, args).skip_norm_wip());
                    while let Some(t) = inner {
                        match t.kind() {
                            TyKind::RawPtr(..) => break,
                            TyKind::Pat(base, _) => inner = Some(base),
                            TyKind::Adt(d, a) => {
                                let AdtId::StructId(s) = d.def_id() else { break };
                                let l = self.layout(cx, t)?;
                                if l.fields.count() == 0 {
                                    break;
                                }
                                ptr_at += l.fields.offset(0).bytes();
                                inner = db
                                    .field_types(s.into())
                                    .iter()
                                    .next()
                                    .map(|(_, f)| f.ty().instantiate(interner, a).skip_norm_wip());
                            }
                            _ => break,
                        }
                    }
                    let wide = matches!(self.pointees[pointee as usize].shape, Shape::Unsized { .. });
                    ops.push(Op::Safe { at: ptr_at, kind: SafePtr::Box { global }, pointee, wide, path });
                    for (i, (_, field)) in fields.iter().enumerate().skip(1) {
                        let fty = field.ty().instantiate(interner, args).skip_norm_wip();
                        let fpath = self.path(path, Elem::Field(VariantId::StructId(id), i as u32));
                        let fl = self.layout(cx, fty)?;
                        self.compile(cx, fty, &fl, at + layout.fields.offset(i).bytes(), fpath, ops, dense)?;
                    }
                }
                AdtId::StructId(id) => {
                    self.fields(cx, VariantId::StructId(id), args, layout, &layout.fields, at, path, ops, dense)?;
                    self.scalar_ranges(cx, layout, at, path, ops);
                }
                AdtId::UnionId(_) => {
                    let start = self.unions.len() as u32;
                    self.union_data(cx, ty, layout, 0)?;
                    let mut data = self.unions.split_off(start as usize);
                    radix::sort_copy_by_key(&mut data, |&(off, len)| radix::pack_64(off, len));
                    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(data.len());
                    for (off, len) in data {
                        match merged.last_mut() {
                            Some(last) if last.0 + last.1 >= off => last.1 = last.1.max(off + len - last.0),
                            _ => merged.push((off, len)),
                        }
                    }
                    *dense &= merged.len() == 1 && merged[0] == (0, layout.size.bytes())
                        || layout.size.bytes() == 0;
                    self.unions.extend(merged);
                    ops.push(Op::Union { at, data: (start, self.unions.len() as u32) });
                }
                AdtId::EnumId(e) => self.enum_ops(cx, ty, e, args, layout, at, path, ops, dense)?,
            },
            TyKind::Coroutine(..) | TyKind::CoroutineClosure(..) | TyKind::CoroutineWitness(..) => {
                // alloc: plan building, once per type (unsupported: an error).
                return Err(Fail::Unsupported(format!(
                    "the validity of a coroutine value, `{}`",
                    type_name(db, ty)
                )));
            }
            TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..) => {
                // alloc: plan building, once per type (unsupported: an error).
                return Err(Fail::Unsupported(format!(
                    "a typed copy of an unsized value, `{}`",
                    type_name(db, ty)
                )));
            }
            TyKind::UnsafeBinder(_)
            | TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Error(_) => {
                // alloc: plan building, once per type (unsupported: an error).
                return Err(Fail::Unsupported(format!(
                    "a value of a type not concrete at run time, `{}`",
                    type_name(db, ty)
                )));
            }
        }
        Ok(())
    }

    /// The fields of a struct or variant, in memory order (`walk_value`'s `in_memory_order`).
    #[allow(clippy::too_many_arguments)]
    fn fields(
        &mut self,
        cx: &TypedCx<'_, 'db>,
        variant: VariantId,
        args: hir_ty::next_solver::GenericArgs<'db>,
        layout: &Layout,
        shape: &FieldsShape<hir_ty::layout::RustcFieldIdx>,
        at: u64,
        path: u32,
        ops: &mut Vec<Op<'db>>,
        dense: &mut bool,
    ) -> R {
        let db = cx.db;
        let interner = DbInterner::new_with(db, cx.env.krate);
        let fields = db.field_types(variant);
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend(0..fields.len() as u32);
        if fields.len() > 0 {
            radix::sort_by_key(&mut order, |&i| shape.offset(i as usize).bytes());
        }
        let _ = layout;
        let result = (|| {
            for &i in &order {
                let field = &fields.as_slice()[i as usize];
                let fty = field.ty().instantiate(interner, args).skip_norm_wip();
                let fpath = self.path(path, Elem::Field(variant, i));
                let fl = self.layout(cx, fty)?;
                self.compile(cx, fty, &fl, at + shape.offset(i as usize).bytes(), fpath, ops, dense)?;
            }
            Ok(())
        })();
        self.order = order;
        result
    }

    /// `visit_scalar` for a layout whose scalars have a narrower valid range than their
    /// primitive (`ty::Pat`, and structs hir-ty gives a range: `NonZero`, `NonNull`).
    fn scalar_ranges(&mut self, cx: &TypedCx<'_, 'db>, layout: &Layout, at: u64, path: u32, ops: &mut Vec<Op<'db>>) {
        let dl = cx.target;
        let mut push = |scalar: Scalar, off: u64| {
            if let Scalar::Initialized { valid_range, .. } = scalar {
                let size = scalar.size(dl);
                if !valid_range.is_full_for(size) {
                    ops.push(Op::Range {
                        at: at + off,
                        size: size.bytes() as u8,
                        start: valid_range.start,
                        end: valid_range.end,
                        path,
                    });
                }
            }
        };
        match layout.backend_repr {
            BackendRepr::Scalar(s) if !s.is_bool() => push(s, 0),
            BackendRepr::ScalarPair { a, b, b_offset } if !a.is_uninit_valid() && !b.is_uninit_valid() => {
                push(a, 0);
                push(b, b_offset.bytes());
            }
            _ => {}
        }
    }

    /// The ops of an enum: the fields of its one variant, or its tag and variants.
    #[allow(clippy::too_many_arguments)]
    fn enum_ops(
        &mut self,
        cx: &TypedCx<'_, 'db>,
        ty: Ty<'db>,
        e: EnumId,
        args: hir_ty::next_solver::GenericArgs<'db>,
        layout: &Layout,
        at: u64,
        path: u32,
        ops: &mut Vec<Op<'db>>,
        dense: &mut bool,
    ) -> R {
        let db = cx.db;
        let variants = &e.enum_variants(db).variants;
        match &layout.variants {
            Variants::Empty => ops.push(Op::Variantless { ty, path }),
            Variants::Single { index } => {
                let Some(&(vid, _)) = variants.get_index(index.0).map(|(_, v)| v) else {
                    // alloc: plan building, once per type.
                    return Err(Fail::Unsupported(format!("variant {} of `{}`", index.0, type_name(db, ty))));
                };
                self.fields(cx, VariantId::EnumVariantId(vid), args, layout, &layout.fields, at, path, ops, dense)?;
            }
            Variants::Multiple { tag, tag_encoding, tag_field, variants: layouts } => {
                *dense = false;
                let tag_at = layout.fields.offset(u32::from(tag_field.0.into_raw()) as usize).bytes();
                let size = tag.size(cx.target);
                let valid = tag.valid_range(cx.target);
                let encoding = match tag_encoding {
                    TagEncoding::Direct => Encoding::Direct,
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => Encoding::Niche {
                        untagged: untagged_variant.0 as u32,
                        start: niche_variants.start.0 as u32,
                        last: niche_variants.last.0 as u32,
                        niche_start: *niche_start,
                    },
                };
                let first = self.variants.len() as u32;
                // Plans of the variants first, so the indices stay contiguous.
                let mut planned: SmallVec<[VariantPlan; 4]> = SmallVec::new();
                for (i, (_, &(vid, _))) in variants.iter().enumerate() {
                    let idx = hir_ty::layout::RustcEnumVariantIdx(i);
                    let vl = Layout::for_variant(layout, idx);
                    let discr = db.const_eval_discriminant(vid).map_err(|err| {
                        // alloc: plan building, once per type.
                        Fail::Unsupported(format!("the discriminant of a variant of `{}`: {err:?}", type_name(db, ty)))
                    })?;
                    // alloc: plan building, once per type.
                    let mut vops = Vec::new();
                    let mut vdense = true;
                    let shape = FieldsShape::Arbitrary {
                        // clone: plan building, once per type.
                        offsets: layouts[idx].field_offsets.clone(),
                        in_memory_order: Default::default(),
                    };
                    self.fields(cx, VariantId::EnumVariantId(vid), args, &vl, &shape, 0, 0, &mut vops, &mut vdense)?;
                    let start = self.ops.len() as u32;
                    self.ops.extend(vops);
                    let plan = self.plans.len() as u32;
                    self.plans.push(Plan { ops: (start, self.ops.len() as u32), size: vl.size.bytes(), dense: false });
                    planned.push(VariantPlan {
                        id: vid,
                        discr: size.truncate(discr as u128),
                        inhabited: !vl.uninhabited,
                        plan,
                    });
                }
                self.variants.extend(planned);
                let plan = self.enums.len() as u32;
                self.enums.push(EnumPlan {
                    tag_at,
                    tag_size: size.bytes() as u8,
                    tag_ptr: matches!(tag.primitive(), Primitive::Pointer(_)),
                    valid_start: valid.start,
                    valid_end: valid.end,
                    encoding,
                    variants: (first, self.variants.len() as u32),
                });
                let tag_path = self.path(path, Elem::EnumTag);
                ops.push(Op::Enum { at, plan, path, tag_path });
            }
        }
        Ok(())
    }

    /// The bytes some field of a union covers, at `base` (validity.rs `union_data_range`).
    fn union_data(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>, layout: &Layout, base: u64) -> R {
        if layout.size.bytes() == 0 {
            return Ok(());
        }
        let db = cx.db;
        let interner = DbInterner::new_with(db, cx.env.krate);
        // The types of the fields of `ty`, in order, for the shapes that have fields.
        let field_tys: SmallVec<[Ty<'db>; 8]> = match ty.kind() {
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => {
                    // alloc: plan building, once per type.
                    db.field_types(id.into()).iter().map(|(_, f)| f.ty().instantiate(interner, args).skip_norm_wip()).collect()
                }
                AdtId::UnionId(id) => {
                    // alloc: plan building, once per type.
                    db.field_types(id.into()).iter().map(|(_, f)| f.ty().instantiate(interner, args).skip_norm_wip()).collect()
                }
                AdtId::EnumId(e) => {
                    // Every variant's fields count (validity.rs: "Don't forget potential other
                    // variants").
                    if let Variants::Multiple { variants: layouts, tag_field, .. } = &layout.variants {
                        let tag_at = layout.fields.offset(u32::from(tag_field.0.into_raw()) as usize).bytes();
                        if let Variants::Multiple { tag, .. } = &layout.variants {
                            self.unions.push((base + tag_at, tag.size(cx.target).bytes()));
                        }
                        for (i, (_, &(vid, _))) in e.enum_variants(db).variants.iter().enumerate() {
                            let idx = hir_ty::layout::RustcEnumVariantIdx(i);
                            let offsets = &layouts[idx].field_offsets;
                            for (j, (_, f)) in db.field_types(VariantId::EnumVariantId(vid)).iter().enumerate() {
                                let fty = f.ty().instantiate(interner, args).skip_norm_wip();
                                let fl = self.layout(cx, fty)?;
                                let off = offsets.raw.get(j).map(|s| s.bytes()).unwrap_or(0);
                                self.union_data(cx, fty, &fl, base + off)?;
                            }
                        }
                        return Ok(());
                    }
                    let Variants::Single { index } = layout.variants else { return Ok(()) };
                    let Some((_, &(vid, _))) = e.enum_variants(db).variants.get_index(index.0) else {
                        return Ok(());
                    };
                    db.field_types(VariantId::EnumVariantId(vid))
                        .iter()
                        .map(|(_, f)| f.ty().instantiate(interner, args).skip_norm_wip())
                        // alloc: plan building, once per type.
                        .collect()
                }
            },
            // alloc: plan building, once per type.
            TyKind::Tuple(fields) => fields.iter().collect(),
            // alloc: plan building, once per type.
            TyKind::Closure(_, args) => args.as_closure().tupled_upvars_ty().tuple_fields().iter().collect(),
            TyKind::Array(elem, _) => {
                if let FieldsShape::Array { stride, count } = layout.fields {
                    let el = self.layout(cx, elem)?;
                    if matches!(el.backend_repr, BackendRepr::Scalar(_)) {
                        self.unions.push((base, el.size.bytes() * count));
                    } else {
                        for i in 0..count {
                            self.union_data(cx, elem, &el, base + i * stride.bytes())?;
                        }
                    }
                }
                return Ok(());
            }
            TyKind::Pat(base_ty, _) => {
                let bl = self.layout(cx, base_ty)?;
                return self.union_data(cx, base_ty, &bl, base);
            }
            _ => SmallVec::new(),
        };
        match &layout.fields {
            FieldsShape::Primitive => self.unions.push((base, layout.size.bytes())),
            FieldsShape::Union(_) => {
                for fty in field_tys {
                    let fl = self.layout(cx, fty)?;
                    self.union_data(cx, fty, &fl, base)?;
                }
            }
            FieldsShape::Arbitrary { .. } | FieldsShape::Array { .. } => {
                for (i, fty) in field_tys.into_iter().enumerate() {
                    let fl = self.layout(cx, fty)?;
                    self.union_data(cx, fty, &fl, base + layout.fields.offset(i).bytes())?;
                }
            }
        }
        Ok(())
    }

    /// What a pointer to `ty` needs of it, computed once.
    fn pointee(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>) -> R<u32> {
        if let Some(&it) = self.pointee_by_ty.get(&ty) {
            return Ok(it);
        }
        let db = cx.db;
        let interner = DbInterner::new_with(db, cx.env.krate);
        let freeze = is_freeze(self.infcx(cx.env, db), ty, cx.env.param_env);
        let unpin = match interner.lang_items().Unpin {
            Some(unpin) => implements_trait_unique(ty, db, cx.env, unpin),
            None => true,
        };
        // Walk to the unsized tail (rustc `struct_tail`), recording each struct level.
        let levels_start = self.levels.len() as u32;
        let cells_start = self.cells.len() as u32;
        let mut cur = ty;
        let mut prefix_at = 0;
        let mut prefix_end;
        let tail = loop {
            prefix_end = self.cells.len() as u32;
            match cur.kind() {
                TyKind::Str => break Some(Tail::Str),
                TyKind::Dynamic(..) => break Some(Tail::Dyn),
                TyKind::Foreign(_) => break Some(Tail::Foreign),
                TyKind::Slice(elem) => {
                    let el = self.layout(cx, elem)?;
                    let s = self.cells.len() as u32;
                    self.sized_cells(cx, elem, &el, 0)?;
                    let elem_cells = (s, self.cells.len() as u32);
                    break Some(Tail::Slice {
                        elem_size: el.size.bytes(),
                        elem_align: el.align.abi.bytes(),
                        elem_cells,
                    });
                }
                TyKind::Adt(def, args) if matches!(def.def_id(), AdtId::StructId(_)) => {
                    let layout = self.layout(cx, cur)?;
                    if layout.is_sized() {
                        break None;
                    }
                    let AdtId::StructId(id) = def.def_id() else { unreachable!() };
                    let fields = db.field_types(id.into());
                    let n = fields.len();
                    let Some((_, last)) = fields.iter().next_back() else { break None };
                    let last = last.ty().instantiate(interner, args).skip_norm_wip();
                    let offset = layout.fields.offset(n - 1).bytes();
                    // The sized fields' cells, at the prefix's offset.
                    for (i, (_, f)) in fields.iter().enumerate().take(n - 1) {
                        let fty = f.ty().instantiate(interner, args).skip_norm_wip();
                        let fl = self.layout(cx, fty)?;
                        self.sized_cells(cx, fty, &fl, prefix_at + layout.fields.offset(i).bytes())?;
                    }
                    self.levels.push((offset, layout.align.abi.bytes()));
                    prefix_at += offset;
                    cur = last;
                }
                TyKind::Tuple(fields) if !fields.is_empty() => {
                    let layout = self.layout(cx, cur)?;
                    if layout.is_sized() {
                        break None;
                    }
                    let n = fields.len();
                    for (i, fty) in fields.iter().enumerate().take(n - 1) {
                        let fl = self.layout(cx, fty)?;
                        self.sized_cells(cx, fty, &fl, prefix_at + layout.fields.offset(i).bytes())?;
                    }
                    let offset = layout.fields.offset(n - 1).bytes();
                    self.levels.push((offset, layout.align.abi.bytes()));
                    prefix_at += offset;
                    cur = fields.as_slice()[n - 1];
                }
                _ => break None,
            }
        };
        let (shape, inhabited) = match tail {
            Some(tail) => (
                Shape::Unsized {
                    levels: (levels_start, self.levels.len() as u32),
                    cells: (cells_start, prefix_end),
                    tail,
                },
                true,
            ),
            None => {
                self.levels.truncate(levels_start as usize);
                self.cells.truncate(cells_start as usize);
                let layout = self.layout(cx, ty)?;
                if !freeze {
                    self.sized_cells(cx, ty, &layout, 0)?;
                }
                let mut cells = self.cells.split_off(cells_start as usize);
                merge(&mut cells);
                self.cells.extend(cells);
                (
                    Shape::Sized {
                        size: layout.size.bytes(),
                        align: layout.align.abi.bytes(),
                        cells: (cells_start, self.cells.len() as u32),
                    },
                    !layout.uninhabited,
                )
            }
        };
        let id = self.pointees.len() as u32;
        self.pointees.push(Pointee { ty, freeze, unpin, inhabited, shape });
        self.pointee_by_ty.insert(ty, id);
        Ok(id)
    }

    /// The `UnsafeCell` ranges of a sized `ty` at `base` (Miri `visit_freeze_sensitive`'s
    /// `UnsafeCellVisitor`).
    fn sized_cells(&mut self, cx: &TypedCx<'_, 'db>, ty: Ty<'db>, layout: &Layout, base: u64) -> R {
        let db = cx.db;
        let interner = DbInterner::new_with(db, cx.env.krate);
        let size = layout.size.bytes();
        if let TyKind::Adt(def, _) = ty.kind()
            && let AdtId::StructId(id) = def.def_id()
            && Some(id) == interner.lang_items().UnsafeCell
        {
            if size != 0 {
                self.cells.push(Span::new(base, size));
            }
            return Ok(());
        }
        if is_freeze(self.infcx(cx.env, db), ty, cx.env.param_env) {
            return Ok(());
        }
        if matches!(layout.fields, FieldsShape::Union(_)) || matches!(layout.variants, Variants::Multiple { .. }) {
            if size != 0 {
                self.cells.push(Span::new(base, size));
            }
            return Ok(());
        }
        let fields: SmallVec<[Ty<'db>; 8]> = match ty.kind() {
            TyKind::Adt(def, args) => {
                let variant = match def.def_id() {
                    AdtId::StructId(id) => VariantId::StructId(id),
                    AdtId::UnionId(id) => VariantId::UnionId(id),
                    AdtId::EnumId(e) => {
                        let Variants::Single { index } = layout.variants else { return Ok(()) };
                        let Some((_, &(vid, _))) = e.enum_variants(db).variants.get_index(index.0) else {
                            return Ok(());
                        };
                        VariantId::EnumVariantId(vid)
                    }
                };
                // alloc: plan building, once per type.
                db.field_types(variant).iter().map(|(_, f)| f.ty().instantiate(interner, args).skip_norm_wip()).collect()
            }
            // alloc: plan building, once per type.
            TyKind::Tuple(fields) => fields.iter().collect(),
            // alloc: plan building, once per type.
            TyKind::Closure(_, args) => args.as_closure().tupled_upvars_ty().tuple_fields().iter().collect(),
            TyKind::Pat(base_ty, _) => {
                let bl = self.layout(cx, base_ty)?;
                return self.sized_cells(cx, base_ty, &bl, base);
            }
            TyKind::Array(elem, _) => {
                if let FieldsShape::Array { stride, count } = layout.fields {
                    let el = self.layout(cx, elem)?;
                    for i in 0..count {
                        self.sized_cells(cx, elem, &el, base + i * stride.bytes())?;
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        for (i, fty) in fields.into_iter().enumerate() {
            let fl = self.layout(cx, fty)?;
            self.sized_cells(cx, fty, &fl, base + layout.fields.offset(i).bytes())?;
        }
        Ok(())
    }
}

/// The data bytes an op covers when nothing at run time decides them, for [`Plan::dense`].
fn op_data(op: &Op<'_>, ptr: u64, out: &mut Vec<(u64, u64)>) {
    match *op {
        Op::Bool { at, .. } => out.push((at, 1)),
        Op::Char { at, .. } => out.push((at, 4)),
        Op::Int { at, size, .. } => out.push((at, size as u64)),
        Op::Scalars { at, len, .. } => out.push((at, len)),
        Op::RawPtr { at, tail, .. } => out.push((at, if tail.is_some() { 2 * ptr } else { ptr })),
        Op::FnPtr { at, .. } => out.push((at, ptr)),
        Op::Safe { at, wide, .. } => out.push((at, if wide { 2 * ptr } else { ptr })),
        Op::Array { at, count, stride, .. } => out.push((at, count * stride)),
        Op::Never { .. } | Op::Variantless { .. } | Op::Range { .. } | Op::Union { .. } | Op::Enum { .. } => {}
    }
}

/// Whether a `Box<T, A>` is of the global allocator (Miri `is_box_global`): `A` is `alloc`'s
/// `Global`, or there is no `A`.
fn box_is_global<'db>(db: &'db Db, args: hir_ty::next_solver::GenericArgs<'db>) -> bool {
    let Some(alloc) = args.as_slice().get(1).and_then(|it| it.ty()) else { return true };
    let TyKind::Adt(def, _) = alloc.kind() else { return false };
    let AdtId::StructId(id) = def.def_id() else { return false };
    matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::Alloc))
        && StructSignature::of(db, id).name.as_str() == "Global"
}

/// Sorts and merges touching ranges.
pub(crate) fn merge(cells: &mut Vec<Span>) {
    radix::sort_copy_by_key(cells, |it| radix::pack_64(it.start, it.len));
    let mut out = 0;
    for i in 0..cells.len() {
        let c = cells[i];
        if c.len == 0 {
            continue;
        }
        if out > 0 && cells[out - 1].end() >= c.start {
            let end = cells[out - 1].end().max(c.end());
            cells[out - 1].len = end - cells[out - 1].start;
        } else {
            cells[out] = c;
            out += 1;
        }
    }
    cells.truncate(out);
}

fn le(bytes: &[u8]) -> u128 {
    let mut word = [0u8; 16];
    word[..bytes.len()].copy_from_slice(bytes);
    u128::from_le_bytes(word)
}

/// A scalar as rustc's `{:x}` of a `Scalar` prints it: `0x` and two digits per byte.
pub(crate) fn hex(v: u128, size: u64) -> String {
    // alloc: error path: the message of a failure.
    format!("{:#0width$x}", v, width = 2 + 2 * size as usize)
}

/// validity.rs `fmt_range`.
pub(crate) fn fmt_range(lo: u128, hi: u128, max_hi: u128) -> String {
    if lo > hi {
        // alloc: error path: the message of a failure.
        format!("less or equal to {hi}, or greater or equal to {lo}")
    } else if lo == hi {
        // alloc: error path: the message of a failure.
        format!("equal to {lo}")
    } else if lo == 0 {
        // alloc: error path: the message of a failure.
        format!("less or equal to {hi}")
    } else if hi == max_hi {
        // alloc: error path: the message of a failure.
        format!("greater or equal to {lo}")
    } else {
        // alloc: error path: the message of a failure.
        format!("in the range {lo}..={hi}")
    }
}

pub(crate) fn type_name<'db>(db: &'db Db, ty: Ty<'db>) -> String {
    // alloc: error path: the message of a failure.
    hir_ty_mir::mir::type_name::rustc_type_name(db, ty).unwrap_or_else(|| format!("{ty:?}"))
}

fn variant_name(db: &Db, v: EnumVariantId) -> String {
    let parent = v.lookup(db).parent;
    parent
        .enum_variants(db)
        .variant_name_by_id(v)
        // alloc: error path: the message of a failure.
        .map(|name| name.as_str().to_owned())
        // alloc: error path: the message of a failure.
        .unwrap_or_else(|| format!("{v:?}"))
}
