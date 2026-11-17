//! An entry point's MIR as one naga entry point (docs/future/gpu/shaders.md, phase 2).
//!
//! Three steps:
//!
//! 1. **Inline** ([`Graph`]). From the entry function, every call is followed into its callee's
//!    monomorphized runtime MIR (`dispatch::body_of`) with the callee the code generator's own
//!    dispatch picks (`dispatch::call`, `mirrors-derive-or-check`), each call site a fresh frame,
//!    into one flat graph of nodes: a MIR block of one frame, the prologue that copies the entry's
//!    arguments in, and the end of the invocation. Recursion is refused. Inlining everything makes a
//!    panic at any depth one branch to the end of the invocation, keeps every resource a global,
//!    and needs no naga pointer arguments.
//! 2. **Structure** ([`crate::structure`]): the flat graph as naga's `if`, `loop`, `break`,
//!    `continue`.
//! 3. **Emit**: each node's statements at its `Run`, its terminator's condition at its `Select`.
//!
//! Values: every MIR local of a value type is a naga local variable, so a value crosses statements,
//! blocks and frames through memory, as naga's scoping of emitted expressions needs; each read is a
//! `Load`. A local of reference type is no variable: it is a symbolic place ([`Sym`]): a root (a
//! local, or a resource global) and steps (fields, and indices snapshotted into a variable of their
//! own where the reference is taken). A reference is bound once, where it is made, as the graph is
//! built; one that would name two places is refused.
//!
//! Panics: a block every path from which ends in a panic (a call of a function the front end
//! models as a panic, `hir_ty_pure::models`), an `unreachable`, or an abort is doomed, and an edge to
//! it, like a failed `assert`, goes to the end of the invocation: a fragment is discarded; a vertex
//! returns its varyings zeroed with its position at (2, 2, 2, 1), outside every clip plane, so
//! nothing it is a corner of is drawn.
use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{AdtId, CallableDefId, EnumId, EnumVariantId, FunctionId, StaticId, VariantId};
use hir_ty::{
    MemoryMap, ParamEnvAndCrate, PointerCast,
    db::HirDatabase as _,
    drop::{DropGlue, has_drop_glue},
    intrinsic::Intrinsic,
    layout::Layout,
    mir as m,
    utils::detect_variant_from_bytes,
    next_solver::{
        ConstKind, DbInterner, GenericArgs, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    dispatch::{self, Body, Call, Callee as Dispatched},
    normalize_or_keep,
};
use hir_ty_pure::models::{Leaf, Model, model_of};
use naga::{
    AddressSpace, ArraySize, BinaryOperator, Binding, BuiltIn, Expression, Handle, ImageClass,
    ImageDimension, Interpolation, Literal, LocalVariable, MathFunction, Sampling, Scalar,
    ScalarKind, Span, Statement, StorageAccess, StructMember, Type, TypeInner, UnaryOperator,
    VectorSize,
};
use rustc_abi::{Endian, FieldsShape};
use rustc_type_ir::{
    FloatTy, IntTy,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _, SliceLike as _},
};

use crate::{
    entries::{self, Entry, FieldKind, Interp, Kind, Out, Stage, Table, ValueTy},
    intrinsics::{self, CoreOp, DeviceFn, DeviceTy, cmath, core_op, device_fn, device_ty},
    refuse::{Refusal, ty_name, why},
    structure::{self, Cfg, Shaped},
};

/// The most frames an entry inlines: past it, a chain of calls that large is refused rather than
/// followed without end (`bounded-progress`).
const MAX_FRAMES: usize = 4096;

/// A refusal before it knows its entry: the construct and the rule.
pub(crate) type Refused = (String, &'static str);

/// What the module's entries share: the database's context, the module being built, and the
/// types and constants already lowered.
pub struct Device<'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnvAndCrate<'db>,
    pub(crate) stored: StoredParamEnvAndCrate,
    pub(crate) little_endian: bool,
    pub module: naga::Module,
    types: HashMap<Ty<'db>, Lowered, Global>,
    consts: HashMap<(Ty<'db>, Box<[u8]>), Handle<naga::Constant>, Global>,
    /// The next resource binding number; each resource of the module has its own, and each
    /// entry's MSL map sends its own to its slots.
    next_binding: u32,
}

/// A Rust type as the device has it.
#[derive(Clone, Debug)]
pub(crate) struct Lowered {
    /// `None` for a type of no size, which has no naga value.
    pub(crate) naga: Option<Handle<Type>>,
    pub(crate) shape: Shape,
}

#[derive(Clone, Debug)]
pub(crate) enum Shape {
    Zst,
    Scalar(Scalar),
    Vector,
    /// A struct or tuple: each Rust field's naga member (`None` for one of no size).
    Struct(Vec<Option<u32>>),
    Array { len: u32 },
    /// An enum value: member 0 its discriminant (an `i32`), then each variant's fields in turn,
    /// apart. Only a value inside a shader has one; no resource holds an enum, so this layout is
    /// the device's own and never meets the host's.
    Enum(Vec<EnumVariant>),
    Texture,
    Sampler,
    /// A `gpu::Tlas`: naga's acceleration structure, a handle like a texture.
    Tlas,
    /// A `gpu::RawOut<T>`: the runtime-sized array of `T` its storage global holds.
    RawOut,
}

/// One variant of a lowered enum.
#[derive(Clone, Debug)]
pub(crate) struct EnumVariant {
    id: EnumVariantId,
    /// Its discriminant, the front end's (`const_eval_discriminant`).
    discr: i128,
    /// Each Rust field's naga member (`None` for one of no size).
    members: Vec<Option<u32>>,
}

/// A symbolic place: what a reference names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sym {
    root: Root,
    steps: Vec<Step>,
    /// For a slice made by unsizing a reference to an array: the array's length, which naga's
    /// `arrayLength` has only for a runtime-sized array.
    len: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    Var(Handle<LocalVariable>),
    Global(Handle<naga::GlobalVariable>),
    /// A place of no size: a reference to it reads and writes nothing.
    Zst,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// A member by its naga index (or a vector's component).
    Member(u32),
    /// An element at the index held in this variable, written where the reference was taken.
    Index(Handle<LocalVariable>),
}

/// What a MIR local of a frame is.
#[derive(Clone, Debug)]
enum Slot {
    /// Not met yet.
    Unset,
    /// A type of no size.
    Zst,
    Var(Handle<LocalVariable>),
    /// A reference, bound to the place it names.
    Ref(Sym),
}

/// One inlined body.
struct Frame<'db> {
    body: &'db m::MirBody<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
    /// The function's path, for errors.
    item: String,
    slots: Vec<Slot>,
    doomed: Vec<bool>,
    /// Each block's successors as the device runs them: no unwind edges, none past a call that
    /// ends the invocation, and only the false arm of a branch on `ub_checks()`.
    succ: Vec<Vec<m::BasicBlockId>>,
    /// The locals that hold `ub_checks()`'s value and are assigned nowhere else: false.
    known_false: Vec<m::LocalId>,
    /// The front end's discriminant temporaries ([`discriminant_temps`]): `i128` in the MIR,
    /// the device's `i32` tag here.
    discriminants: Vec<m::LocalId>,
    /// What each block's call runs, decided once.
    calls: HashMap<m::BasicBlockId, Target<'db>, Global>,
    nodes: HashMap<m::BasicBlockId, u32, Global>,
    /// The caller: its frame, where the value goes, and the node it returns to.
    caller: Option<(usize, m::StoredPlace, u32)>,
}

/// What a call runs.
#[derive(Clone, Debug)]
enum Target<'db> {
    /// It ends the invocation.
    Panic,
    Inline(FunctionId, GenericArgs<'db>),
    Device(DeviceFn),
    /// A naga math function (a core intrinsic or one of std's libm calls), with its arity.
    Math(MathFunction, usize),
    /// Any other core intrinsic the device has.
    Core(CoreOp),
    /// `Clone::clone` of a type whose clone the compiler supplies: a copy of `*self`.
    CloneCopy,
    /// An intrinsic with no effect at run time (`assume`, `cold_path`).
    Nothing,
    /// `ub_checks()`: false. Its value may be either; a program without UB runs the same with
    /// both, and the device build checks nothing it doesn't have to. The branch it guards is
    /// pruned before the graph is built, so the checks' bodies (`const_eval_select`, pointer
    /// arithmetic) are never lowered.
    False,
    Refused(String, &'static str),
}

/// A node of the flat graph.
#[derive(Clone, Debug)]
struct Node {
    kind: NodeKind,
    succ: Vec<u32>,
}

#[derive(Clone, Debug)]
enum NodeKind {
    /// Copies the entry's arguments into its parameters.
    Prologue,
    /// The end of the invocation.
    End,
    Block { frame: usize, block: m::BasicBlockId },
}

/// The node every doomed edge goes to.
const END: u32 = 1;

/// One entry being lowered.
struct EntryCx<'d, 'db> {
    dev: &'d mut Device<'db>,
    entry: &'d Entry,
    frames: Vec<Frame<'db>>,
    nodes: Vec<Node>,
    /// The frame each inlined call site made, by its frame and block.
    callees: HashMap<(usize, m::BasicBlockId), usize, Global>,
    func: naga::Function,
    /// Snapshot variables of indices, by the statement that takes the reference and the step.
    snapshots: HashMap<(usize, m::BasicBlockId, usize, usize), Handle<LocalVariable>, Global>,
    /// The statements that take a reference, with the snapshots each writes.
    snapshot_writes: HashMap<(usize, m::BasicBlockId, usize), Vec<(Handle<LocalVariable>, m::LocalId)>, Global>,
    blocks: Vec<naga::Block>,
    flag: Option<Handle<LocalVariable>>,
    /// The entry's arguments and result: their naga types.
    inputs: Vec<Input>,
    result: Option<Handle<Type>>,
    refusals: Vec<Refusal>,
}

/// What one entry parameter is on the device.
#[derive(Clone, Debug)]
enum Input {
    /// A built-in or the varyings: argument `arg` of the naga function, of this I/O type.
    Arg { arg: u32, io: Handle<Type> },
    /// A resource.
    Global(Handle<naga::GlobalVariable>),
    /// A kernel's `Thread`: the naga arguments of its global, local and workgroup ids.
    Thread { global: u32, local: u32, group: u32 },
}

impl<'db> Device<'db> {
    pub fn new(db: &'db Db, krate: base_db::Crate) -> Device<'db> {
        let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate };
        let interner = DbInterner::new_with(db, krate);
        let little_endian = !matches!(db.target_data_layout(krate).map(|dl| dl.endian), Ok(Endian::Big));
        Device {
            db,
            interner,
            infcx: interner.infer_ctxt().build(TypingMode::Codegen),
            env,
            stored: env.store(),
            little_endian,
            module: naga::Module::default(),
            types: HashMap::default(),  // alloc: the module's types
            consts: HashMap::default(), // alloc: the module's constants
            next_binding: 0,
        }
    }

    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }

    /// `ty` as a refusal names it.
    fn shown(&self, ty: Ty<'db>) -> String {
        ty_name(self.db, self.env.krate, ty)
    }

    /// A constant of `ty` with `have` bytes where its layout needs `want`.
    fn cut(&self, ty: Ty<'db>, have: usize, want: usize) -> Refused {
        // alloc: an error
        (format!("a constant of `{}` with {have} bytes, where its layout needs {want}", self.shown(ty)), why::OPERATIONS)
    }

    /// `f` of the front end's layout of `ty`, the one native codegen uses.
    pub(crate) fn with_layout<R>(&self, ty: Ty<'db>, f: impl FnOnce(&Layout) -> R) -> Result<R, Refused> {
        let layout = self
            .db
            .layout_of_ty(ty.store(), self.stored.clone()) // clone: a stored handle
            // alloc: an error
            .map_err(|e| (format!("a value of `{ty}`, which has no layout ({e:?})", ty = self.shown(ty)), why::TYPES))?;
        Ok(f(&layout))
    }

    fn insert_ty(&mut self, name: Option<String>, inner: TypeInner) -> Handle<Type> {
        self.module.types.insert(Type { name, inner }, Span::UNDEFINED)
    }

    /// `ty` as the device has it.
    pub(crate) fn lower_ty(&mut self, ty: Ty<'db>) -> Result<Lowered, Refused> {
        let ty = self.normalize(ty);
        if let Some(found) = self.types.get(&ty) {
            return Ok(found.clone()); // clone: a small description
        }
        let lowered = self.lower_ty_uncached(ty)?;
        self.types.insert(ty, lowered.clone()); // clone: a small description
        Ok(lowered)
    }

    fn scalar(&mut self, scalar: Scalar) -> Lowered {
        Lowered { naga: Some(self.insert_ty(None, TypeInner::Scalar(scalar))), shape: Shape::Scalar(scalar) }
    }

    fn lower_ty_uncached(&mut self, ty: Ty<'db>) -> Result<Lowered, Refused> {
        let size = |dev: &Self| dev.with_layout(ty, |l| l.size.bytes());
        Ok(match ty.kind() {
            TyKind::Bool => self.scalar(Scalar::BOOL),
            TyKind::Int(_) | TyKind::Uint(_) => {
                let signed = matches!(ty.kind(), TyKind::Int(_));
                match size(self)? {
                    4 => self.scalar(if signed { Scalar::I32 } else { Scalar::U32 }),
                    // alloc: an error
                    n => return Err((format!("the {}-bit integer type `{ty}`", n * 8, ty = self.shown(ty)), why::WIDTH)),
                }
            }
            TyKind::Float(FloatTy::F32) => self.scalar(Scalar::F32),
            TyKind::Float(_) => return Err((format!("the float type `{ty}`", ty = self.shown(ty)), why::TYPES)), // alloc: an error
            TyKind::Never => Lowered { naga: None, shape: Shape::Zst },
            TyKind::Tuple(fields) => {
                let fields: Vec<Ty<'db>> = fields.iter().collect(); // alloc: a tuple's field types
                self.lower_struct(ty, None, &fields)?
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                match device_ty(self.db, adt) {
                    Some(DeviceTy::Vector { scalar, size }) => {
                        let naga = self.insert_ty(None, TypeInner::Vector { size, scalar });
                        Lowered { naga: Some(naga), shape: Shape::Vector }
                    }
                    Some(DeviceTy::Texture2d) => {
                        let class = ImageClass::Sampled { kind: ScalarKind::Float, multi: false };
                        let inner = TypeInner::Image { dim: ImageDimension::D2, arrayed: false, class };
                        Lowered { naga: Some(self.insert_ty(None, inner)), shape: Shape::Texture }
                    }
                    Some(DeviceTy::Sampler) => {
                        let inner = TypeInner::Sampler { comparison: false };
                        Lowered { naga: Some(self.insert_ty(None, inner)), shape: Shape::Sampler }
                    }
                    Some(DeviceTy::Tlas) => {
                        let inner = TypeInner::AccelerationStructure { vertex_return: false };
                        Lowered { naga: Some(self.insert_ty(None, inner)), shape: Shape::Tlas }
                    }
                    Some(DeviceTy::RawOut) => {
                        let elem = self.normalize(args.type_at(0));
                        let stride = self.with_layout(elem, |l| l.size.bytes())? as u32;
                        // alloc: an error
                        let base = self.lower_ty(elem)?.naga.ok_or_else(|| (format!("the RawOut `{ty}` of elements of no size", ty = self.shown(ty)), why::TYPES))?;
                        let inner = TypeInner::Array { base, size: ArraySize::Dynamic, stride };
                        Lowered { naga: Some(self.insert_ty(None, inner)), shape: Shape::RawOut }
                    }
                    None => match adt {
                        AdtId::StructId(id) => {
                            let variant = VariantId::StructId(id);
                            let mut fields = Vec::new(); // alloc: a struct's field types
                            for (_, field) in self.db.field_types(variant).iter() {
                                let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                                fields.push(self.normalize(field));
                            }
                            let name = hir_def::signatures::StructSignature::of(self.db, id).name.as_str().to_owned(); // alloc: a type's name
                            self.lower_struct(ty, Some(name), &fields)?
                        }
                        AdtId::EnumId(id) => self.lower_enum(ty, id, args)?,
                        AdtId::UnionId(_) => return Err((format!("the union `{ty}`", ty = self.shown(ty)), why::TYPES)), // alloc: an error
                    },
                }
            }
            TyKind::Array(elem, _) => {
                let (count, stride) = self.with_layout(ty, |l| match l.fields {
                    FieldsShape::Array { count, stride } => Some((count, stride.bytes())),
                    _ => None,
                })?
                .ok_or_else(|| (format!("the array `{ty}`, laid out as no array", ty = self.shown(ty)), why::TYPES))?; // alloc: an error
                let elem_lowered = self.lower_ty(elem)?;
                match (elem_lowered.naga, std::num::NonZeroU32::new(count as u32)) {
                    (Some(base), Some(len)) => {
                        let inner = TypeInner::Array { base, size: ArraySize::Constant(len), stride: stride as u32 };
                        Lowered { naga: Some(self.insert_ty(None, inner)), shape: Shape::Array { len: len.get() } }
                    }
                    _ => Lowered { naga: None, shape: Shape::Zst },
                }
            }
            TyKind::Ref(..) | TyKind::RawPtr(..) => {
                // alloc: an error
                return Err((format!("a reference or pointer `{ty}` held in a value", ty = self.shown(ty)), why::POINTERS));
            }
            _ => return Err((format!("the type `{ty}`", ty = self.shown(ty)), why::TYPES)), // alloc: an error
        })
    }

    /// A struct or tuple: its members at the front end's offsets, in the order of their offsets
    /// (naga's), the ones of no size left out.
    fn lower_struct(&mut self, ty: Ty<'db>, name: Option<String>, fields: &[Ty<'db>]) -> Result<Lowered, Refused> {
        let (size, offsets) = self.with_layout(ty, |l| {
            let offsets: Vec<u64> = (0..fields.len()).map(|i| l.fields.offset(i).bytes()).collect(); // alloc: offsets
            (l.size.bytes(), offsets)
        })?;
        if size == 0 {
            return Ok(Lowered { naga: None, shape: Shape::Zst });
        }
        let mut present = Vec::new(); // alloc: a struct's members
        for (i, field) in fields.iter().enumerate() {
            if let Some(member_ty) = self.lower_ty(*field)?.naga {
                present.push((offsets[i], i, member_ty));
            }
        }
        present.sort_by_key(|(offset, i, _)| (*offset, *i));
        let mut map = vec![None; fields.len()]; // alloc: the field map
        let mut members = Vec::with_capacity(present.len()); // alloc: naga's members
        for (k, (offset, i, member_ty)) in present.into_iter().enumerate() {
            map[i] = Some(k as u32);
            members.push(StructMember { name: Some(format!("f{i}")), ty: member_ty, binding: None, offset: offset as u32 }); // alloc: a name
        }
        let naga = self.insert_ty(name, TypeInner::Struct { members, span: size as u32 });
        Ok(Lowered { naga: Some(naga), shape: Shape::Struct(map) })
    }

    /// An enum: its discriminant, then every variant's fields, each member 16 bytes past the end
    /// of the one before (aligned for any naga type). The variants share no storage: a value
    /// inside a shader needs no layout of the host's, and a union of members is what naga lacks.
    fn lower_enum(&mut self, ty: Ty<'db>, id: EnumId, args: GenericArgs<'db>) -> Result<Lowered, Refused> {
        if self.with_layout(ty, |l| l.size.bytes())? == 0 {
            return Ok(Lowered { naga: None, shape: Shape::Zst });
        }
        let tag = self.insert_ty(None, TypeInner::Scalar(Scalar::I32));
        let mut members = vec![StructMember { name: Some("tag".to_owned()), ty: tag, binding: None, offset: 0 }]; // alloc: an enum's members
        let mut offset = 16u32;
        let mut variants = Vec::new(); // alloc: an enum's variants
        let all: Vec<EnumVariantId> = id.enum_variants(self.db).variants.values().map(|(v, _)| *v).collect(); // alloc: the variants
        for (vi, variant) in all.into_iter().enumerate() {
            let discr = self
                .db
                .const_eval_discriminant(variant)
                // alloc: an error
                .map_err(|e| (format!("the discriminant of a variant of `{ty}`, which did not evaluate ({e:?})", ty = self.shown(ty)), why::ENUMS))?;
            // The tag is an `i32`, and so is every discriminant read on the device
            // (`EntryCx::local_ty`): a wider one would be cut.
            if i32::try_from(discr).is_err() {
                // alloc: an error
                return Err((format!("the enum `{ty}`, whose discriminant {discr} does not fit the device's 32-bit tag", ty = self.shown(ty)), why::ENUMS));
            }
            let mut map = Vec::new(); // alloc: a variant's members
            let fields: Vec<Ty<'db>> = self
                .db
                .field_types(VariantId::EnumVariantId(variant))
                .iter()
                .map(|(_, f)| self.normalize(f.ty().instantiate(self.interner, args).skip_norm_wip()))
                .collect(); // alloc: a variant's field types
            for (k, field) in fields.into_iter().enumerate() {
                match self.lower_ty(field)?.naga {
                    Some(member_ty) => {
                        let size = self.module.types[member_ty].inner.size(self.module.to_ctx());
                        map.push(Some(members.len() as u32));
                        // alloc: a name
                        members.push(StructMember { name: Some(format!("v{vi}_{k}")), ty: member_ty, binding: None, offset });
                        offset += size.next_multiple_of(16);
                    }
                    None => map.push(None),
                }
            }
            variants.push(EnumVariant { id: variant, discr, members: map });
        }
        let naga = self.insert_ty(None, TypeInner::Struct { members, span: offset });
        Ok(Lowered { naga: Some(naga), shape: Shape::Enum(variants) })
    }

    /// The constant of type `ty` with these bytes, in the module's global expressions.
    fn constant(&mut self, ty: Ty<'db>, bytes: &[u8]) -> Result<Option<Handle<naga::Constant>>, Refused> {
        let ty = self.normalize(ty);
        if let Some(found) = self.consts.get(&(ty, Box::from(bytes))) {
            return Ok(Some(*found));
        }
        let lowered = self.lower_ty(ty)?;
        let Some(naga_ty) = lowered.naga else { return Ok(None) };
        // A constant is as many bytes as its type's layout: every part below is read at the
        // offsets that layout gives.
        let size = self.with_layout(ty, |l| l.size.bytes_usize())?;
        if bytes.len() != size {
            return Err(self.cut(ty, bytes.len(), size));
        }
        let init = self.const_expr(ty, &lowered, bytes)?;
        let handle = self.module.constants.append(naga::Constant { name: None, ty: naga_ty, init }, Span::UNDEFINED);
        self.consts.insert((ty, Box::from(bytes)), handle); // alloc: the key
        Ok(Some(handle))
    }

    fn const_expr(&mut self, ty: Ty<'db>, lowered: &Lowered, bytes: &[u8]) -> Result<Handle<Expression>, Refused> {
        let naga_ty = lowered.naga.expect("a constant of no size is not built");
        let expr = match &lowered.shape {
            Shape::Scalar(scalar) => Expression::Literal(self.literal(*scalar, bytes)?),
            Shape::Vector => {
                let TypeInner::Vector { size, scalar } = self.module.types[naga_ty].inner else { unreachable!() };
                let width = scalar.width as usize;
                let mut components = Vec::new(); // alloc: a vector constant's components
                for i in 0..size as usize {
                    let lit = self.literal(scalar, &bytes[i * width..(i + 1) * width])?;
                    components.push(self.module.global_expressions.append(Expression::Literal(lit), Span::UNDEFINED));
                }
                Expression::Compose { ty: naga_ty, components }
            }
            Shape::Struct(map) => {
                let fields = self.field_tys(ty)?;
                let offsets = self.with_layout(ty, |l| (0..fields.len()).map(|i| l.fields.offset(i).bytes() as usize).collect::<Vec<_>>())?; // alloc: offsets
                let mut members: Vec<(u32, Handle<Expression>)> = Vec::new(); // alloc: a constant's members
                for (i, field) in fields.iter().enumerate() {
                    let Some(k) = map[i] else { continue };
                    let field_lowered = self.lower_ty(*field)?;
                    let size = self.with_layout(*field, |l| l.size.bytes() as usize)?;
                    let part = bytes.get(offsets[i]..offsets[i] + size).ok_or_else(|| self.cut(ty, bytes.len(), offsets[i] + size))?;
                    members.push((k, self.const_expr(*field, &field_lowered, part)?));
                }
                members.sort_by_key(|(k, _)| *k);
                Expression::Compose { ty: naga_ty, components: members.into_iter().map(|(_, e)| e).collect() }
            }
            Shape::Array { len } => {
                let TyKind::Array(elem, _) = ty.kind() else { unreachable!() };
                let elem_lowered = self.lower_ty(elem)?;
                let stride = self.with_layout(elem, |l| l.size.bytes() as usize)?;
                let mut components = Vec::new(); // alloc: an array constant's elements
                for i in 0..*len as usize {
                    let part = bytes.get(i * stride..(i + 1) * stride).ok_or_else(|| self.cut(ty, bytes.len(), (i + 1) * stride))?;
                    components.push(self.const_expr(elem, &elem_lowered, part)?);
                }
                Expression::Compose { ty: naga_ty, components }
            }
            Shape::Enum(variants) => {
                let TyKind::Adt(def, args) = ty.kind() else { unreachable!("an enum shape is an enum") };
                let AdtId::EnumId(id) = def.def_id() else { unreachable!("an enum shape is an enum") };
                let dl = self
                    .db
                    .target_data_layout(self.env.krate)
                    .map_err(|e| (format!("the device target's data layout ({e:?})"), why::ENUMS))?; // alloc: an error
                let db = self.db;
                let found = self.with_layout(ty, |layout| {
                    detect_variant_from_bytes(layout, db, dl, bytes, id).map(|(variant, variant_layout)| {
                        let offsets: Vec<usize> = (0..variant_layout.fields.count()) // alloc: a variant's offsets
                            .map(|i| variant_layout.fields.offset(i).bytes() as usize)
                            .collect();
                        (variant, offsets)
                    })
                })?;
                // alloc: an error
                let (variant, offsets) = found.ok_or_else(|| (format!("a constant of `{ty}` whose bytes are no variant", ty = self.shown(ty)), why::ENUMS))?;
                let at = variants.iter().position(|v| v.id == variant).expect("a variant of the enum");
                let TypeInner::Struct { members, .. } = &self.module.types[naga_ty].inner else { unreachable!() };
                let member_tys: Vec<Handle<Type>> = members.iter().map(|m| m.ty).collect(); // alloc: member types
                let mut components: Vec<Option<Handle<Expression>>> = vec![None; member_tys.len()]; // alloc: members
                let tag = Expression::Literal(Literal::I32(variants[at].discr as i32));
                components[0] = Some(self.module.global_expressions.append(tag, Span::UNDEFINED));
                let fields: Vec<Ty<'db>> = self
                    .db
                    .field_types(VariantId::EnumVariantId(variant))
                    .iter()
                    .map(|(_, f)| self.normalize(f.ty().instantiate(self.interner, args).skip_norm_wip()))
                    .collect(); // alloc: field types
                for (i, field) in fields.iter().enumerate() {
                    let Some(k) = variants[at].members[i] else { continue };
                    let field_lowered = self.lower_ty(*field)?;
                    let size = self.with_layout(*field, |l| l.size.bytes() as usize)?;
                    let part = bytes.get(offsets[i]..offsets[i] + size).ok_or_else(|| self.cut(ty, bytes.len(), offsets[i] + size))?;
                    components[k as usize] = Some(self.const_expr(*field, &field_lowered, part)?);
                }
                let components = components
                    .into_iter()
                    .zip(member_tys)
                    .map(|(c, t)| c.unwrap_or_else(|| self.module.global_expressions.append(Expression::ZeroValue(t), Span::UNDEFINED)))
                    .collect();
                Expression::Compose { ty: naga_ty, components }
            }
            Shape::Zst | Shape::Texture | Shape::Sampler | Shape::Tlas | Shape::RawOut => {
                return Err((format!("a constant of type `{ty}`", ty = self.shown(ty)), why::OPERATIONS)); // alloc: an error
            }
        };
        Ok(self.module.global_expressions.append(expr, Span::UNDEFINED))
    }

    fn literal(&self, scalar: Scalar, bytes: &[u8]) -> Result<Literal, Refused> {
        // alloc: an error
        let cut = || (format!("a scalar constant of {} bytes, read as one of {}", bytes.len(), scalar.width), why::OPERATIONS);
        let word: [u8; 4] = match scalar.width {
            4 => bytes.try_into().map_err(|_| cut())?,
            1 => [bytes.first().copied().ok_or_else(cut)?, 0, 0, 0],
            _ => return Err(("a scalar constant of an unknown width".to_owned(), why::WIDTH)), // alloc: an error
        };
        let bits = if self.little_endian || scalar.width == 1 { u32::from_le_bytes(word) } else { u32::from_be_bytes(word) };
        Ok(match scalar.kind {
            ScalarKind::Bool => Literal::Bool(bits != 0),
            ScalarKind::Sint => Literal::I32(bits as i32),
            ScalarKind::Uint => Literal::U32(bits),
            ScalarKind::Float => Literal::F32(f32::from_bits(bits)),
            _ => return Err(("a constant of an abstract type".to_owned(), why::TYPES)), // alloc: an error
        })
    }

    /// The field types of a struct or tuple type.
    fn field_tys(&self, ty: Ty<'db>) -> Result<Vec<Ty<'db>>, Refused> {
        Ok(match ty.kind() {
            TyKind::Tuple(fields) => fields.iter().collect(), // alloc: field types
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => self
                    .db
                    .field_types(VariantId::StructId(id))
                    .iter()
                    .map(|(_, f)| self.normalize(f.ty().instantiate(self.interner, args).skip_norm_wip()))
                    .collect(), // alloc: field types
                _ => return Err((format!("the fields of `{ty}`", ty = self.shown(ty)), why::ENUMS)), // alloc: an error
            },
            _ => return Err((format!("the fields of `{ty}`", ty = self.shown(ty)), why::TYPES)), // alloc: an error
        })
    }

    /// A resource global, with the next binding number.
    fn resource(&mut self, name: String, space: AddressSpace, ty: Handle<Type>) -> Handle<naga::GlobalVariable> {
        let binding = naga::ResourceBinding { group: 0, binding: self.next_binding };
        self.next_binding += 1;
        self.module.global_variables.append(
            naga::GlobalVariable {
                name: Some(name),
                space,
                binding: Some(binding),
                ty,
                init: None,
                memory_decorations: naga::MemoryDecorations::empty(),
            },
            Span::UNDEFINED,
        )
    }

    /// Lowers `entry` into the module as one of its entry points: the resource binding of each
    /// of its resource parameters, with the slot its descriptor binds it at; every refusal met.
    pub fn lower_entry(&mut self, entry: &Entry) -> Result<Vec<(naga::ResourceBinding, entries::Slot)>, Vec<Refusal>> {
        let mut cx = EntryCx {
            dev: self,
            entry,
            frames: Vec::new(),
            nodes: vec![
                Node { kind: NodeKind::Prologue, succ: Vec::new() },
                Node { kind: NodeKind::End, succ: Vec::new() },
            ], // alloc: the graph
            callees: HashMap::default(), // alloc: per entry
            func: naga::Function { name: Some(entry.name.clone()), ..Default::default() }, // clone: the entry's name
            snapshots: HashMap::default(),       // alloc: per entry
            snapshot_writes: HashMap::default(), // alloc: per entry
            blocks: Vec::new(),
            flag: None,
            inputs: Vec::new(),
            result: None,
            refusals: Vec::new(),
        };
        cx.run();
        if !cx.refusals.is_empty() {
            return Err(cx.refusals);
        }
        let (stage, workgroup_size) = match entry.stage {
            Stage::Vertex => (naga::ShaderStage::Vertex, [0, 0, 0]),
            Stage::Fragment => (naga::ShaderStage::Fragment, [0, 0, 0]),
            Stage::Compute { x, y, z } => (naga::ShaderStage::Compute, [x, y, z]),
        };
        let function = std::mem::take(&mut cx.func);
        let mut bindings = Vec::new(); // alloc: an entry's binding table
        for (input, param) in cx.inputs.iter().zip(&entry.params) {
            if let Input::Global(g) = input
                && let Some(binding) = &cx.dev.module.global_variables[*g].binding
            {
                bindings.push((binding.clone(), param.slot)); // clone: a small binding
            }
        }
        self.module.entry_points.push(naga::EntryPoint {
            name: entry.name.clone(), // clone: the entry's name
            stage,
            early_depth_test: None,
            workgroup_size,
            workgroup_size_overrides: None,
            function,
            mesh_info: None,
            task_payload: None,
            incoming_ray_payload: None,
        });
        Ok(bindings)
    }
}

/// What a place is, once its pointer is built.
enum Ptr<'db> {
    /// A pointer to a value of this type.
    Pointer(Handle<Expression>, Ty<'db>),
    /// A texture or sampler: the handle itself.
    Handle(Handle<Expression>),
    /// A place of no size.
    Zst,
}

impl<'d, 'db> EntryCx<'d, 'db> {
    fn refuse_in(&mut self, item: &str, (construct, why): Refused) {
        self.refusals.push(Refusal {
            entry: self.entry.name.clone(), // clone: the entry's name, per refusal
            item: item.to_owned(),          // alloc: per refusal
            construct,
            why,
        });
    }

    fn refuse(&mut self, frame: usize, refused: Refused) {
        let item = self.frames[frame].item.clone(); // clone: the item's path, per refusal
        self.refuse_in(&item, refused);
    }

    fn entry_item(&self) -> String {
        intrinsics::fn_path(self.dev.db, self.entry.func)
    }

    fn run(&mut self) {
        let root = match self.push_frame(self.entry.func, GenericArgs::new_from_slice(&[]), None) {
            Ok(frame) => frame,
            Err(refused) => {
                let item = self.entry_item();
                return self.refuse_in(&item, refused);
            }
        };
        if let Err(refused) = self.interface(root) {
            let item = self.entry_item();
            return self.refuse_in(&item, refused);
        }
        let start = self.node_of(root, self.frames[root].body.start_block);
        self.nodes[0].succ = vec![start]; // alloc: one successor
        self.walk_frame(root);
        if !self.refusals.is_empty() {
            return;
        }
        let cfg = Cfg { succ: self.nodes.iter().map(|n| n.succ.clone()).collect() }; // clone: the graph's edges
        let shaped = match structure::structured(&cfg) {
            Ok(shaped) => shaped,
            Err(e) => {
                let item = self.entry_item();
                return self.refuse_in(&item, (e.to_string(), why::CONTROL));
            }
        };
        self.blocks.push(naga::Block::new());
        self.emit_all(&shaped);
        // Nothing falls off the end: the structured code exits on every path, and this keeps
        // naga's writers from seeing a function that may end without a value.
        self.end_invocation();
        let body = self.blocks.pop().expect("the function's block");
        self.func.body = body;
    }

    // ---- The graph ----

    fn push_frame(
        &mut self,
        func: FunctionId,
        args: GenericArgs<'db>,
        caller: Option<(usize, m::StoredPlace, u32)>,
    ) -> Result<usize, Refused> {
        let db = self.dev.db;
        let item = intrinsics::fn_path(db, func);
        if self.frames.len() >= MAX_FRAMES {
            // alloc: an error
            return Err((format!("a call of `{item}`, past {MAX_FRAMES} inlined calls"), why::RECURSION));
        }
        let mut at = caller.as_ref().map(|(f, ..)| *f);
        while let Some(f) = at {
            if self.frames[f].func == func && self.frames[f].args == args {
                return Err((format!("a call of `{item}` from inside itself"), why::RECURSION)); // alloc: an error
            }
            at = self.frames[f].caller.as_ref().map(|(f, ..)| *f);
        }
        let body = dispatch::body_of(db, self.dev.stored.clone(), Body::Fn(func), args.store()) // clone: a stored handle
            .map_err(|e| (format!("the body of `{item}`, which did not lower to MIR ({e:?})"), why::NO_BODY))?; // alloc: an error
        if !body.closures.is_empty() {
            return Err((format!("a closure in `{item}`"), why::CALLS)); // alloc: an error
        }
        let n = body.locals.len();
        self.frames.push(Frame {
            body,
            func,
            args,
            item,
            slots: vec![Slot::Unset; n], // alloc: a frame's locals
            doomed: Vec::new(),
            succ: Vec::new(),
            known_false: Vec::new(),
            discriminants: discriminant_temps(body),
            calls: HashMap::default(), // alloc: a frame's calls
            nodes: HashMap::default(), // alloc: a frame's nodes
            caller,
        });
        let frame = self.frames.len() - 1;
        self.doom(frame);
        Ok(frame)
    }

    fn local_index(local: m::LocalId) -> usize {
        local.into_raw().into_u32() as usize
    }

    fn block_index(block: m::BasicBlockId) -> usize {
        block.into_raw().into_u32() as usize
    }

    /// What each call of `frame` runs, its successors as the device runs them, and its doomed
    /// blocks: the least set closed under "every successor is doomed", from the blocks that end
    /// in a panic, `unreachable` or an abort.
    fn doom(&mut self, frame: usize) {
        let body = self.frames[frame].body;
        let n = body.basic_blocks.len();
        for (id, block) in body.basic_blocks.iter() {
            if let Some(term) = &block.terminator
                && let m::TerminatorKind::Call { func, .. } = &term.kind
            {
                let call = self.target(func);
                self.frames[frame].calls.insert(id, call);
            }
        }
        // `ub_checks()`'s results, each assigned only by its call.
        let mut known_false = Vec::new(); // alloc: a frame's ub_checks results
        for (id, block) in body.basic_blocks.iter() {
            if let Some(term) = &block.terminator
                && let m::TerminatorKind::Call { destination, .. } = &term.kind
                && matches!(self.frames[frame].calls.get(&id), Some(Target::False))
                && destination.projection.as_slice().is_empty()
            {
                known_false.push(destination.local);
            }
        }
        known_false.retain(|&local| assignments(body, local) == 1);
        self.frames[frame].known_false = known_false;
        let mut succ: Vec<Vec<m::BasicBlockId>> = vec![Vec::new(); n]; // alloc: a frame's edges
        let mut doomed = vec![false; n]; // alloc: a frame's doomed blocks
        for (id, block) in body.basic_blocks.iter() {
            let i = Self::block_index(id);
            let Some(term) = &block.terminator else { continue };
            match &term.kind {
                m::TerminatorKind::Unreachable | m::TerminatorKind::Abort | m::TerminatorKind::UnwindResume => {
                    doomed[i] = true
                }
                m::TerminatorKind::Call { target, .. } => match self.frames[frame].calls.get(&id) {
                    Some(Target::Panic | Target::Device(DeviceFn::Discard)) => doomed[i] = true,
                    _ => succ[i] = target.iter().copied().collect(), // alloc: edges
                },
                m::TerminatorKind::SwitchInt { discr, targets } if self.is_known_false(frame, discr) => {
                    succ[i] = vec![false_target(targets)] // alloc: an edge
                }
                kind => succ[i] = normal_successors(kind),
            }
        }
        // A block with no successor is a return or a diverging call: not doomed here (a
        // diverging callee ends the invocation itself).
        for _round in 0..=n {
            let mut changed = false;
            for i in 0..n {
                if !doomed[i] && !succ[i].is_empty() && succ[i].iter().all(|&t| doomed[Self::block_index(t)]) {
                    doomed[i] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.frames[frame].doomed = doomed;
        self.frames[frame].succ = succ;
    }

    /// Whether `op` reads a local that holds `ub_checks()`'s false.
    fn is_known_false(&self, frame: usize, op: &m::Operand) -> bool {
        match &op.kind {
            m::OperandKind::Copy(place) | m::OperandKind::Move(place) => {
                place.projection.as_slice().is_empty() && self.frames[frame].known_false.contains(&place.local)
            }
            _ => false,
        }
    }

    /// What a call's callee runs, by the code generator's dispatch.
    fn target(&mut self, func: &m::Operand) -> Target<'db> {
        let db = self.dev.db;
        let Some((CallableDefId::FunctionId(f), args)) = func.fn_def() else {
            return Target::Refused("a call of a function pointer, closure or constructor".to_owned(), why::CALLS); // alloc: an error
        };
        match dispatch::call(&self.dev.infcx, self.dev.env, f, args) {
            Call::Intrinsic(intrinsic) => match intrinsic {
                Intrinsic::Assume | Intrinsic::ColdPath => Target::Nothing,
                Intrinsic::UbChecks => Target::False,
                Intrinsic::AbortImmediate | Intrinsic::Unreachable => Target::Panic,
                other => match core_op(other) {
                    Some(CoreOp::Math(fun, arity)) => Target::Math(fun, arity),
                    Some(op) => Target::Core(op),
                    None => Target::Refused(format!("the intrinsic `{other:?}`"), why::OPERATIONS), // alloc: an error
                },
            },
            Call::Method(Ok(Dispatched::Fn(g, g_args))) => {
                if let Some(device) = device_fn(db, g) {
                    // Decided here, not at the edge: a discard's block is doomed, so its edges are
                    // never walked.
                    if device == DeviceFn::Discard && matches!(self.entry.stage, Stage::Compute { .. }) {
                        return Target::Refused("`Discard` in a kernel".to_owned(), why::STAGE); // alloc: an error
                    }
                    return Target::Device(device);
                }
                if model_of(db, g) == Some(Model::Leaf(Leaf::Panic)) {
                    return Target::Panic;
                }
                if let Some((fun, arity)) = cmath(db, g) {
                    return Target::Math(fun, arity);
                }
                let sig = hir_def::signatures::FunctionSignature::of(db, g);
                if !sig.has_body() || matches!(g.loc(db).container, hir_def::ItemContainerId::ExternBlockId(_)) {
                    return Target::Refused(format!("a call of `{}`", intrinsics::fn_path(db, g)), why::NO_BODY); // alloc: an error
                }
                Target::Inline(g, g_args)
            }
            Call::Method(Ok(Dispatched::CloneBuiltin(_))) => Target::CloneCopy,
            Call::Method(Ok(Dispatched::Virtual)) | Call::Indirect => {
                Target::Refused("a call through `dyn` or a function pointer".to_owned(), why::CALLS) // alloc: an error
            }
            Call::Method(Ok(other)) => Target::Refused(format!("a call the compiler supplies the body of ({other:?})"), why::NO_BODY), // alloc: an error
            Call::Method(Err(e)) => Target::Refused(format!("a trait method no impl is found for ({e:?})"), why::CALLS), // alloc: an error
            Call::Closure(..) | Call::FnItem(..) | Call::AsyncClosure => {
                Target::Refused("a call through `Fn*`".to_owned(), why::CALLS) // alloc: an error
            }
            Call::DropGlue(_) => Target::Refused("drop glue".to_owned(), why::OPERATIONS), // alloc: an error
            Call::ConstEvalSelect(_) => Target::Refused("`const_eval_select`".to_owned(), why::OPERATIONS), // alloc: an error
            Call::UnknownIntrinsic(u) => Target::Refused(format!("the unknown intrinsic {u:?}"), why::OPERATIONS), // alloc: an error
        }
    }

    /// The node of a block of `frame`: the end, if it is doomed.
    fn node_of(&mut self, frame: usize, block: m::BasicBlockId) -> u32 {
        if self.frames[frame].doomed[Self::block_index(block)] {
            return END;
        }
        if let Some(&node) = self.frames[frame].nodes.get(&block) {
            return node;
        }
        let node = self.nodes.len() as u32;
        self.nodes.push(Node { kind: NodeKind::Block { frame, block }, succ: Vec::new() });
        self.frames[frame].nodes.insert(block, node);
        node
    }

    /// Each block of `frame` reached from its start, in reverse postorder (so a reference is bound
    /// before a block it dominates uses it), its edges and its references.
    fn walk_frame(&mut self, frame: usize) {
        let body = self.frames[frame].body;
        for block in reverse_postorder(body.start_block, &self.frames[frame].succ, &self.frames[frame].doomed) {
            if self.refusals.len() > 64 {
                return;
            }
            let node = self.node_of(frame, block);
            if node == END {
                continue;
            }
            let data = &body.basic_blocks[block];
            for (si, statement) in data.statements.iter().enumerate() {
                if let Err(refused) = self.bind_statement(frame, block, si, &statement.kind) {
                    self.refuse(frame, refused);
                }
            }
            let succ = match self.edges(frame, block) {
                Ok(succ) => succ,
                Err(refused) => {
                    self.refuse(frame, refused);
                    Vec::new()
                }
            };
            self.nodes[node as usize].succ = succ;
        }
    }

    /// A statement that makes a reference binds it.
    fn bind_statement(&mut self, frame: usize, block: m::BasicBlockId, si: usize, kind: &m::StatementKind) -> Result<(), Refused> {
        let m::StatementKind::Assign(place, rvalue) = kind else { return Ok(()) };
        if !place.projection.as_slice().is_empty() {
            if self.is_ref(self.place_ty(frame, place)) {
                return Err(("a reference stored into a field or element".to_owned(), why::POINTERS)); // alloc: an error
            }
            return Ok(());
        }
        let dest_ty = self.local_ty(frame, place.local);
        if !self.is_ref(dest_ty) {
            return Ok(());
        }
        let sym = match rvalue {
            m::Rvalue::Ref(_, of) => self.sym_of(frame, of, Some((block, si)))?,
            m::Rvalue::Use(op) => match &op.kind {
                m::OperandKind::Copy(of) | m::OperandKind::Move(of) => self.sym_of_ref_value(frame, of)?,
                _ => return Err(self.reference_constant(op, "a reference constant")),
            },
            m::Rvalue::CopyForDeref(of) => self.sym_of_ref_value(frame, of)?,
            m::Rvalue::Cast(kind, op, to) => self.cast_sym(frame, kind, op, self.dev.normalize(to.as_ref()))?,
            _ => return Err(("a reference made other than by `&`".to_owned(), why::POINTERS)), // alloc: an error
        };
        self.bind(frame, place.local, sym)
    }

    fn bind(&mut self, frame: usize, local: m::LocalId, sym: Sym) -> Result<(), Refused> {
        let slot = &mut self.frames[frame].slots[Self::local_index(local)];
        match slot {
            Slot::Unset => {
                *slot = Slot::Ref(sym);
                Ok(())
            }
            Slot::Ref(bound) if *bound == sym => Ok(()),
            _ => Err((format!("the reference `_{}`, which names a different place on another path", Self::local_index(local)), why::POINTERS)), // alloc: an error
        }
    }

    /// Whether `ty` is a reference or raw pointer: a symbolic place, not a value.
    fn is_ref(&self, ty: Ty<'db>) -> bool {
        matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
    }

    /// The place a pointer cast's operand names, as the cast's result names it: the same place
    /// for a cast that keeps what it points at (`&T` to `*const T`, `*mut T` to `*const T`), the
    /// array's place with its length for an unsizing of `&[T; N]` to `&[T]`.
    fn cast_sym(&mut self, frame: usize, kind: &m::CastKind, op: &m::Operand, to: Ty<'db>) -> Result<Sym, Refused> {
        let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &op.kind else {
            return Err(("a cast of a pointer constant".to_owned(), why::POINTERS)); // alloc: an error
        };
        let from = self.place_ty(frame, of);
        let (Some(from_pointee), Some(to_pointee)) = (pointee(from), pointee(to)) else {
            return Err((format!("a cast from `{from}` to `{to}`", from = self.dev.shown(from), to = self.dev.shown(to)), why::POINTERS)); // alloc: an error
        };
        let (from_pointee, to_pointee) = (self.dev.normalize(from_pointee), self.dev.normalize(to_pointee));
        let mut sym = self.sym_of_ref_value(frame, of)?;
        match kind {
            m::CastKind::PointerCoercion(PointerCast::Unsize) => match (from_pointee.kind(), to_pointee.kind()) {
                (TyKind::Array(..), TyKind::Slice(_)) => {
                    sym.len = match self.dev.lower_ty(from_pointee)?.shape {
                        Shape::Array { len } => Some(len),
                        _ => Some(0),
                    };
                    Ok(sym)
                }
                // alloc: an error
                _ => Err((format!("an unsizing of `{from}` to `{to}` (only an array to a slice lowers)", from = self.dev.shown(from), to = self.dev.shown(to)), why::POINTERS)),
            },
            m::CastKind::PtrToPtr | m::CastKind::PointerCoercion(PointerCast::MutToConstPointer) if from_pointee == to_pointee => Ok(sym),
            // alloc: an error
            _ => Err((format!("a `{kind:?}` cast from `{from}` to `{to}`, which changes what it points at", from = self.dev.shown(from), to = self.dev.shown(to)), why::POINTERS)),
        }
    }

    fn local_ty(&self, frame: usize, local: m::LocalId) -> Ty<'db> {
        if self.frames[frame].discriminants.contains(&local) {
            // It only ever holds a discriminant, which is the device's `i32` tag: every enum
            // lowered has discriminants that fit it (`lower_enum`), and any other type's is 0.
            return Ty::new_int(self.dev.interner, IntTy::I32);
        }
        self.dev.normalize(self.frames[frame].body.locals[local].ty.as_ref())
    }

    fn place_ty(&self, frame: usize, place: &m::StoredPlace) -> Ty<'db> {
        if place.projection.as_slice().is_empty() {
            return self.local_ty(frame, place.local);
        }
        let ty = place.as_ref().ty(self.frames[frame].body, &self.dev.infcx, self.dev.env.param_env).ty;
        self.dev.normalize(ty)
    }

    /// The place a whole reference-typed place holds: its binding.
    fn sym_of_ref_value(&mut self, frame: usize, of: &m::StoredPlace) -> Result<Sym, Refused> {
        if !of.projection.as_slice().is_empty() {
            return Err(("a reference read out of a field or element".to_owned(), why::POINTERS)); // alloc: an error
        }
        match &self.frames[frame].slots[Self::local_index(of.local)] {
            Slot::Ref(sym) => Ok(sym.clone()), // clone: a small place
            _ => Err((format!("the reference `_{}`, read before it is bound", Self::local_index(of.local)), why::POINTERS)), // alloc: an error
        }
    }

    /// The symbolic place `of` names; `at` the statement taking the reference, where index
    /// snapshots are written.
    fn sym_of(&mut self, frame: usize, of: &m::StoredPlace, at: Option<(m::BasicBlockId, usize)>) -> Result<Sym, Refused> {
        let elems = of.projection.as_slice();
        let (mut sym, ty, rest) = match elems.first() {
            Some(m::ProjectionElem::Deref) => {
                let sym = self.sym_of_ref_value(frame, &m::Place::from(of.local).store())?;
                // alloc: an error
                let inner = pointee(self.local_ty(frame, of.local)).ok_or_else(|| ("a deref of a non-reference".to_owned(), why::POINTERS))?;
                (sym, self.dev.normalize(inner), &elems[1..])
            }
            _ => {
                let root = match self.var(frame, of.local)? {
                    Some(var) => Root::Var(var),
                    None => Root::Zst,
                };
                (Sym { root, steps: Vec::new(), len: None }, self.local_ty(frame, of.local), elems)
            }
        };
        let mut pty = m::PlaceTy::from_ty(ty);
        for (step_index, elem) in rest.iter().enumerate() {
            let step = match elem {
                m::ProjectionElem::Field(field) => Some(Step::Member(self.member(pty.ty, pty.variant_id, field.0 as usize)?)),
                m::ProjectionElem::Downcast(_) => None,
                m::ProjectionElem::ConstantIndex { offset, from_end: false, .. } => Some(Step::Member(*offset as u32)),
                m::ProjectionElem::Index(index) => {
                    let Some((block, si)) = at else {
                        return Err(("an index in a place not taken by reference".to_owned(), why::POINTERS)); // alloc: an error
                    };
                    let key = (frame, block, si, step_index);
                    let snap = match self.snapshots.get(&key) {
                        Some(h) => *h,
                        None => {
                            let u32_ty = self.dev.lower_ty(self.usize_ty())?.naga.expect("usize has a size");
                            let h = self.func.local_variables.append(LocalVariable { name: Some("index".to_owned()), ty: u32_ty, init: None }, Span::UNDEFINED); // alloc: a name
                            self.snapshots.insert(key, h);
                            self.snapshot_writes.entry((frame, block, si)).or_default().push((h, *index));
                            h
                        }
                    };
                    Some(Step::Index(snap))
                }
                m::ProjectionElem::Deref => return Err(("a reference through a reference held in a field".to_owned(), why::POINTERS)), // alloc: an error
                _ => return Err((format!("the projection `{elem:?}`"), why::OPERATIONS)), // alloc: an error
            };
            if sym.root != Root::Zst {
                sym.steps.extend(step);
            }
            pty = pty.projection_ty(&self.dev.infcx, elem, self.dev.env.param_env);
            pty.ty = self.dev.normalize(pty.ty);
        }
        if !rest.is_empty() {
            sym.len = None;
        }
        if sym.root != Root::Zst && self.dev.lower_ty(pty.ty)?.naga.is_none() {
            // A place of no size inside a value: nothing to name.
            sym = Sym { root: Root::Zst, steps: Vec::new(), len: None };
        }
        Ok(sym)
    }

    fn usize_ty(&self) -> Ty<'db> {
        Ty::new_uint(self.dev.interner, rustc_type_ir::UintTy::Usize)
    }

    /// The naga member of field `field` of `ty`, of its variant `variant` for an enum.
    fn member(&mut self, ty: Ty<'db>, variant: Option<VariantId>, field: usize) -> Result<u32, Refused> {
        // The database and crate, not `self`: `lower_ty` below borrows the device mutably.
        let (db, krate) = (self.dev.db, self.dev.env.krate);
        let none = || (format!("field {field} of `{ty}`, which has no size", ty = ty_name(db, krate, ty)), why::TYPES); // alloc: an error
        match (self.dev.lower_ty(ty)?.shape, variant) {
            (Shape::Vector, _) => Ok(field as u32),
            (Shape::Struct(map), _) => map.get(field).copied().flatten().ok_or_else(none),
            (Shape::Enum(variants), Some(VariantId::EnumVariantId(v))) => variants
                .iter()
                .find(|x| x.id == v)
                .and_then(|x| x.members.get(field).copied().flatten())
                .ok_or_else(none),
            _ => Err((format!("a field of `{ty}`", ty = self.dev.shown(ty)), why::TYPES)), // alloc: an error
        }
    }

    /// The variable of a value local (made on first use); `None` for one of no size.
    fn var(&mut self, frame: usize, local: m::LocalId) -> Result<Option<Handle<LocalVariable>>, Refused> {
        let i = Self::local_index(local);
        match &self.frames[frame].slots[i] {
            Slot::Var(h) => return Ok(Some(*h)),
            Slot::Zst => return Ok(None),
            Slot::Ref(_) => return Err((format!("the reference `_{i}` used as a value"), why::POINTERS)), // alloc: an error
            Slot::Unset => {}
        }
        let ty = self.local_ty(frame, local);
        if self.is_ref(ty) {
            return Err((format!("the reference `_{i}`, used before it is bound"), why::POINTERS)); // alloc: an error
        }
        let lowered = self.dev.lower_ty(ty)?;
        let slot = match lowered.naga {
            None => Slot::Zst,
            Some(naga_ty) => {
                if matches!(lowered.shape, Shape::Texture | Shape::Sampler | Shape::Tlas | Shape::RawOut) {
                    return Err((format!("a `{ty}` held by value", ty = self.dev.shown(ty)), why::TYPES)); // alloc: an error
                }
                let name = format!("{}_{i}", short_name(&self.frames[frame].item)); // alloc: a name
                let init = Some(self.func.expressions.append(Expression::ZeroValue(naga_ty), Span::UNDEFINED));
                Slot::Var(self.func.local_variables.append(LocalVariable { name: Some(name), ty: naga_ty, init }, Span::UNDEFINED))
            }
        };
        let out = match &slot {
            Slot::Var(h) => Some(*h),
            _ => None,
        };
        self.frames[frame].slots[i] = slot;
        Ok(out)
    }

    /// A block's successors, and the frames its call inlines.
    fn edges(&mut self, frame: usize, block: m::BasicBlockId) -> Result<Vec<u32>, Refused> {
        let body = self.frames[frame].body;
        let Some(term) = &body.basic_blocks[block].terminator else {
            return Err(("a block without a terminator".to_owned(), why::CONTROL)); // alloc: an error
        };
        Ok(match &term.kind {
            m::TerminatorKind::Goto { target }
            | m::TerminatorKind::FalseEdge { real_target: target, .. }
            | m::TerminatorKind::FalseUnwind { real_target: target, .. } => vec![self.node_of(frame, *target)], // alloc: an edge
            m::TerminatorKind::SwitchInt { discr, targets } if self.is_known_false(frame, discr) => {
                vec![self.node_of(frame, false_target(targets))] // alloc: an edge
            }
            m::TerminatorKind::SwitchInt { targets, .. } => {
                let mut out: Vec<u32> = targets.iter().map(|(_, t)| t).collect::<Vec<_>>().into_iter().map(|t| self.node_of(frame, t)).collect(); // alloc: edges
                out.push(self.node_of(frame, targets.otherwise()));
                out
            }
            m::TerminatorKind::Assert { target, .. } => vec![self.node_of(frame, *target), END], // alloc: edges
            m::TerminatorKind::Drop { place, target, .. } => {
                let ty = self.place_ty(frame, place);
                if !matches!(has_drop_glue(&self.dev.infcx, ty, self.dev.env.param_env), DropGlue::None) {
                    return Err((format!("a drop of a `{ty}`, which runs code", ty = self.dev.shown(ty)), why::OPERATIONS)); // alloc: an error
                }
                vec![self.node_of(frame, *target)] // alloc: an edge
            }
            m::TerminatorKind::Return => match self.frames[frame].caller.clone() {
                // clone: the caller's place
                None => Vec::new(),
                Some((caller, dest, after)) => {
                    let ret = m::return_slot();
                    if self.is_ref(self.local_ty(frame, ret)) {
                        if !dest.projection.as_slice().is_empty() {
                            return Err(("a returned reference stored into a field".to_owned(), why::POINTERS)); // alloc: an error
                        }
                        let sym = self.sym_of_ref_value(frame, &m::Place::from(ret).store())?;
                        self.bind(caller, dest.local, sym)?;
                    }
                    vec![after] // alloc: an edge
                }
            },
            m::TerminatorKind::Call { args, destination, target, .. } => {
                let call = self.frames[frame].calls.get(&block).cloned().expect("decided when dooming"); // clone: a small decision
                let after = target.map(|t| self.node_of(frame, t));
                match call {
                    Target::Refused(construct, why) => return Err((construct, why)),
                    Target::Panic | Target::Device(DeviceFn::Discard) => vec![END], // alloc: an edge
                    Target::Device(device) => {
                        if device.fragment_only() && self.entry.stage != Stage::Fragment {
                            let stage = if self.entry.stage == Stage::Vertex { "a vertex stage" } else { "a kernel" };
                            return Err((format!("`{device:?}` in {stage}"), why::STAGE)); // alloc: an error
                        }
                        after.into_iter().collect()
                    }
                    Target::Math(..) | Target::Core(_) | Target::CloneCopy | Target::Nothing | Target::False => after.into_iter().collect(),
                    Target::Inline(g, g_args) => {
                        let Some(after) = after else {
                            // A diverging call: its callee never returns here.
                            let callee = self.push_frame(g, g_args, None)?;
                            self.callees.insert((frame, block), callee);
                            self.bind_args(frame, callee, args)?;
                            let start = self.node_of(callee, self.frames[callee].body.start_block);
                            self.walk_frame(callee);
                            return Ok(vec![start]); // alloc: an edge
                        };
                        let callee = self.push_frame(g, g_args, Some((frame, destination.clone(), after)))?; // clone: the destination
                        self.callees.insert((frame, block), callee);
                        self.bind_args(frame, callee, args)?;
                        let start = self.node_of(callee, self.frames[callee].body.start_block);
                        self.walk_frame(callee);
                        vec![start] // alloc: an edge
                    }
                }
            }
            m::TerminatorKind::Unreachable | m::TerminatorKind::Abort | m::TerminatorKind::UnwindResume => vec![END], // alloc: an edge
            other => return Err((format!("the terminator `{other:?}`"), why::CONTROL)), // alloc: an error
        })
    }

    /// The callee's parameters bound to the caller's reference arguments; value arguments are
    /// stored when the call's block runs.
    fn bind_args(&mut self, caller: usize, callee: usize, args: &[m::Operand]) -> Result<(), Refused> {
        let params = self.frames[callee].body.param_locals.clone(); // clone: a few locals
        if params.len() != args.len() {
            return Err(("a call whose arguments are not its callee's parameters (a rust-call spread)".to_owned(), why::CALLS)); // alloc: an error
        }
        for (param, arg) in params.iter().zip(args) {
            if !self.is_ref(self.local_ty(callee, *param)) {
                continue;
            }
            let sym = match &arg.kind {
                m::OperandKind::Copy(of) | m::OperandKind::Move(of) => self.sym_of_ref_value(caller, of)?,
                _ => return Err(self.reference_constant(arg, "a reference constant passed to a call")),
            };
            self.bind(callee, *param, sym)?;
        }
        Ok(())
    }

    // ---- The interface ----

    /// The entry's arguments and result, checked against its descriptor, and its reference
    /// parameters bound to their resources.
    fn interface(&mut self, root: usize) -> Result<(), Refused> {
        let entry = self.entry;
        let params = self.frames[root].body.param_locals.clone(); // clone: a few locals
        if params.len() != entry.params.len() {
            // alloc: an error
            return Err((format!("{} parameters where its descriptor has {}", params.len(), entry.params.len()), why::INTERFACE));
        }
        let mut args = 0u32;
        for (local, param) in params.iter().zip(&entry.params) {
            let ty = self.local_ty(root, *local);
            let input = match &param.kind {
                Kind::VertexIndex | Kind::InstanceIndex => {
                    let built_in = if param.kind == Kind::VertexIndex { BuiltIn::VertexIndex } else { BuiltIn::InstanceIndex };
                    let io = self.value_ty(ValueTy::U32);
                    self.func.arguments.push(naga::FunctionArgument { name: Some(param.name.clone()), ty: io, binding: Some(Binding::BuiltIn(built_in)) }); // clone: a name
                    args += 1;
                    Input::Arg { arg: args - 1, io }
                }
                Kind::Varyings(fields) => {
                    let io = self.io_struct(&format!("{}_in", entry.name), fields, false); // alloc: a name
                    self.func.arguments.push(naga::FunctionArgument { name: Some(param.name.clone()), ty: io, binding: None }); // clone: a name
                    args += 1;
                    Input::Arg { arg: args - 1, io }
                }
                Kind::Uniform { size } => {
                    let TyKind::Ref(_, inner, _) = ty.kind() else { return Err((format!("uniform `{}` is not a `&T`", param.name), why::INTERFACE)) }; // alloc: an error
                    self.host_shared(inner, &param.name)?;
                    let actual = self.dev.with_layout(inner, |l| l.size.bytes())?;
                    if actual != u64::from(*size) {
                        return Err((format!("uniform `{}`: its descriptor says {size} bytes, its type has {actual}", param.name), why::INTERFACE)); // alloc: an error
                    }
                    let naga_ty = self.dev.lower_ty(inner)?.naga.ok_or_else(|| (format!("uniform `{}` of no size", param.name), why::INTERFACE))?; // alloc: an error
                    let g = self.dev.resource(format!("{}_{}", entry.name, param.name), AddressSpace::Uniform, naga_ty); // alloc: a name
                    self.bind(root, *local, Sym { root: Root::Global(g), steps: Vec::new(), len: None })?;
                    Input::Global(g)
                }
                Kind::Storage { stride } => {
                    let elem = match ty.kind() {
                        TyKind::Ref(_, inner, _) => match inner.kind() {
                            TyKind::Slice(elem) => elem,
                            _ => return Err((format!("storage `{}` is not a `&[T]`", param.name), why::INTERFACE)), // alloc: an error
                        },
                        _ => return Err((format!("storage `{}` is not a `&[T]`", param.name), why::INTERFACE)), // alloc: an error
                    };
                    self.host_shared(elem, &param.name)?;
                    let actual = self.dev.with_layout(elem, |l| l.size.bytes())?;
                    if actual != u64::from(*stride) {
                        return Err((format!("storage `{}`: its descriptor says a stride of {stride}, its type has {actual}", param.name), why::INTERFACE)); // alloc: an error
                    }
                    let base = self.dev.lower_ty(elem)?.naga.ok_or_else(|| (format!("storage `{}` of elements of no size", param.name), why::INTERFACE))?; // alloc: an error
                    let array = self.dev.insert_ty(None, TypeInner::Array { base, size: ArraySize::Dynamic, stride: *stride });
                    let g = self.dev.resource(format!("{}_{}", entry.name, param.name), AddressSpace::Storage { access: StorageAccess::LOAD }, array); // alloc: a name
                    self.bind(root, *local, Sym { root: Root::Global(g), steps: Vec::new(), len: None })?;
                    Input::Global(g)
                }
                Kind::Texture2d | Kind::Sampler | Kind::Tlas => {
                    let TyKind::Ref(_, inner, _) = ty.kind() else { return Err((format!("`{}` is not a reference", param.name), why::INTERFACE)) }; // alloc: an error
                    let naga_ty = self.dev.lower_ty(inner)?.naga.expect("textures, samplers and acceleration structures have a type");
                    let g = self.dev.resource(format!("{}_{}", entry.name, param.name), AddressSpace::Handle, naga_ty); // alloc: a name
                    self.bind(root, *local, Sym { root: Root::Global(g), steps: Vec::new(), len: None })?;
                    Input::Global(g)
                }
                Kind::RawOut { stride } => {
                    let elem = match ty.kind() {
                        TyKind::Ref(_, inner, _) => match inner.kind() {
                            TyKind::Adt(def, args) if device_ty(self.dev.db, def.def_id()) == Some(DeviceTy::RawOut) => self.dev.normalize(args.type_at(0)),
                            _ => return Err((format!("RawOut `{}` is not a `&RawOut<T>`", param.name), why::INTERFACE)), // alloc: an error
                        },
                        _ => return Err((format!("RawOut `{}` is not a `&RawOut<T>`", param.name), why::INTERFACE)), // alloc: an error
                    };
                    self.host_shared(elem, &param.name)?;
                    let actual = self.dev.with_layout(elem, |l| l.size.bytes())?;
                    if actual != u64::from(*stride) {
                        return Err((format!("RawOut `{}`: its descriptor says a stride of {stride}, its type has {actual}", param.name), why::INTERFACE)); // alloc: an error
                    }
                    let TyKind::Ref(_, inner, _) = ty.kind() else { unreachable!("matched above") };
                    let array = self.dev.lower_ty(inner)?.naga.expect("a RawOut has its array type");
                    let access = StorageAccess::LOAD | StorageAccess::STORE;
                    let g = self.dev.resource(format!("{}_{}", entry.name, param.name), AddressSpace::Storage { access }, array); // alloc: a name
                    self.bind(root, *local, Sym { root: Root::Global(g), steps: Vec::new(), len: None })?;
                    Input::Global(g)
                }
                Kind::Thread => {
                    let io = self.dev.insert_ty(None, TypeInner::Vector { size: VectorSize::Tri, scalar: Scalar::U32 });
                    for (suffix, built_in) in [("global", BuiltIn::GlobalInvocationId), ("local", BuiltIn::LocalInvocationId), ("group", BuiltIn::WorkGroupId)] {
                        // alloc: a name
                        self.func.arguments.push(naga::FunctionArgument { name: Some(format!("{}_{suffix}", param.name)), ty: io, binding: Some(Binding::BuiltIn(built_in)) });
                    }
                    args += 3;
                    Input::Thread { global: args - 3, local: args - 2, group: args - 1 }
                }
            };
            let want_table = match param.kind {
                Kind::Uniform { .. } | Kind::Storage { .. } | Kind::RawOut { .. } | Kind::Tlas => Table::Buffer,
                Kind::Texture2d => Table::Texture,
                Kind::Sampler => Table::Sampler,
                _ => Table::None,
            };
            if param.slot.table != want_table {
                return Err((format!("parameter `{}` is bound in the {:?} table, its kind in {want_table:?}", param.name, param.slot.table), why::INTERFACE)); // alloc: an error
            }
            self.inputs.push(input);
        }
        let ret_ty = self.local_ty(root, m::return_slot());
        let result = match &entry.out {
            Out::Nothing => {
                if self.dev.lower_ty(ret_ty)?.naga.is_some() {
                    return Err((format!("a kernel returning `{ret_ty}`, not `()`", ret_ty = self.dev.shown(ret_ty)), why::INTERFACE)); // alloc: an error
                }
                return Ok(());
            }
            Out::Color => {
                let vec4 = self.value_ty(ValueTy::Vec4);
                if self.dev.lower_ty(ret_ty)?.naga != Some(vec4) {
                    return Err((format!("a fragment stage returning `{ret_ty}`, not a `Vec4`", ret_ty = self.dev.shown(ret_ty)), why::INTERFACE)); // alloc: an error
                }
                let binding = Binding::Location { location: 0, interpolation: None, sampling: None, blend_src: None, per_primitive: false };
                self.func.result = Some(naga::FunctionResult { ty: vec4, binding: Some(binding) });
                vec4
            }
            Out::Varyings(fields) => {
                let io = self.io_struct(&format!("{}_out", entry.name), fields, true); // alloc: a name
                self.func.result = Some(naga::FunctionResult { ty: io, binding: None });
                io
            }
        };
        self.result = Some(result);
        Ok(())
    }

    /// That a resource's type holds no enum: an enum's naga layout is the device's own (see
    /// [`Shape::Enum`]), so one in memory the host writes would read other bytes than it wrote.
    fn host_shared(&mut self, ty: Ty<'db>, param: &str) -> Result<(), Refused> {
        let ty = self.dev.normalize(ty);
        match ty.kind() {
            TyKind::Adt(def, _) if matches!(def.def_id(), AdtId::EnumId(_)) => {
                // alloc: an error
                Err((format!("resource `{param}` holding the enum `{ty}`, whose device layout is not the host's", ty = self.dev.shown(ty)), why::ENUMS))
            }
            TyKind::Array(elem, _) => self.host_shared(elem, param),
            TyKind::Adt(..) | TyKind::Tuple(..) => {
                if let TyKind::Adt(def, _) = ty.kind()
                    && device_ty(self.dev.db, def.def_id()).is_some()
                {
                    return Ok(());
                }
                for field in self.dev.field_tys(ty)? {
                    self.host_shared(field, param)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn value_ty(&mut self, ty: ValueTy) -> Handle<Type> {
        let (scalar, size) = match ty {
            ValueTy::F32 => (Scalar::F32, None),
            ValueTy::Vec2 => (Scalar::F32, Some(VectorSize::Bi)),
            ValueTy::Vec3 => (Scalar::F32, Some(VectorSize::Tri)),
            ValueTy::Vec4 => (Scalar::F32, Some(VectorSize::Quad)),
            ValueTy::U32 => (Scalar::U32, None),
            ValueTy::UVec4 => (Scalar::U32, Some(VectorSize::Quad)),
            ValueTy::I32 => (Scalar::I32, None),
            ValueTy::IVec2 => (Scalar::I32, Some(VectorSize::Bi)),
        };
        let inner = match size {
            None => TypeInner::Scalar(scalar),
            Some(size) => TypeInner::Vector { size, scalar },
        };
        self.dev.insert_ty(None, inner)
    }

    /// The I/O struct of a varyings struct: a member per field in its descriptor's order, each
    /// at its location or the position built-in, 16 bytes apart.
    fn io_struct(&mut self, name: &str, fields: &[entries::Field], output: bool) -> Handle<Type> {
        let mut members = Vec::with_capacity(fields.len()); // alloc: an I/O struct's members
        for (i, field) in fields.iter().enumerate() {
            let (ty, binding) = match field.kind {
                FieldKind::Position => (self.value_ty(ValueTy::Vec4), Binding::BuiltIn(BuiltIn::Position { invariant: false })),
                FieldKind::Value { ty, interp } => {
                    let (interpolation, sampling) = match interp {
                        Interp::Perspective => (Interpolation::Perspective, Some(Sampling::Center)),
                        Interp::Linear => (Interpolation::Linear, Some(Sampling::Center)),
                        Interp::Flat => (Interpolation::Flat, None),
                    };
                    let location = field.location.unwrap_or(0);
                    (self.value_ty(ty), Binding::Location { location, interpolation: Some(interpolation), sampling, blend_src: None, per_primitive: false })
                }
            };
            let _ = output;
            members.push(StructMember { name: Some(field.name.clone()), ty, binding: Some(binding), offset: 16 * i as u32 }); // clone: a name
        }
        let span = 16 * fields.len() as u32;
        self.dev.insert_ty(Some(name.to_owned()), TypeInner::Struct { members, span }) // alloc: a name
    }

    // ---- Emitting ----

    fn block(&mut self) -> &mut naga::Block {
        self.blocks.last_mut().expect("a block is open")
    }

    fn stmt(&mut self, statement: Statement) {
        self.block().push(statement, Span::UNDEFINED);
    }

    /// An expression in the current block: emitted where naga needs it to be.
    fn expr(&mut self, expr: Expression) -> Handle<Expression> {
        let pre_emitted = matches!(
            expr,
            Expression::Literal(_)
                | Expression::Constant(_)
                | Expression::ZeroValue(_)
                | Expression::FunctionArgument(_)
                | Expression::GlobalVariable(_)
                | Expression::LocalVariable(_)
        );
        let at = self.func.expressions.len();
        let handle = self.func.expressions.append(expr, Span::UNDEFINED);
        if !pre_emitted {
            let range = self.func.expressions.range_from(at);
            self.stmt(Statement::Emit(range));
        }
        handle
    }

    fn emit_all(&mut self, shaped: &[Shaped]) {
        for s in shaped {
            if self.refusals.len() > 64 {
                return;
            }
            match s {
                Shaped::Run(b) => self.run_node(*b),
                Shaped::Exit(b) => self.exit_node(*b),
                Shaped::Select { block, arms } => self.select(*block, arms),
                Shaped::Loop(body) => {
                    let body = self.nested(body);
                    self.stmt(Statement::Loop { body, continuing: naga::Block::new(), break_if: None });
                }
                Shaped::Break => self.stmt(Statement::Break),
                Shaped::Continue => self.stmt(Statement::Continue),
                Shaped::SetFlag(value) => {
                    let flag = self.flag();
                    let pointer = self.expr(Expression::LocalVariable(flag));
                    let value = self.expr(Expression::Literal(Literal::U32(*value)));
                    self.stmt(Statement::Store { pointer, value });
                }
                Shaped::ClearFlag => {
                    let flag = self.flag();
                    let pointer = self.expr(Expression::LocalVariable(flag));
                    let value = self.expr(Expression::Literal(Literal::U32(0)));
                    self.stmt(Statement::Store { pointer, value });
                }
                Shaped::IfFlag { value, then } => {
                    let condition = self.flag_is(BinaryOperator::Equal, *value);
                    let accept = self.nested(then);
                    self.stmt(Statement::If { condition, accept, reject: naga::Block::new() });
                }
                Shaped::IfAnyFlag(then) => {
                    let condition = self.flag_is(BinaryOperator::NotEqual, 0);
                    let accept = self.nested(then);
                    self.stmt(Statement::If { condition, accept, reject: naga::Block::new() });
                }
            }
        }
    }

    fn nested(&mut self, shaped: &[Shaped]) -> naga::Block {
        self.blocks.push(naga::Block::new());
        self.emit_all(shaped);
        self.blocks.pop().expect("the nested block")
    }

    fn flag(&mut self) -> Handle<LocalVariable> {
        if let Some(flag) = self.flag {
            return flag;
        }
        let u32_ty = self.value_ty(ValueTy::U32);
        let init = Some(self.func.expressions.append(Expression::Literal(Literal::U32(0)), Span::UNDEFINED));
        let flag = self.func.local_variables.append(LocalVariable { name: Some("flag".to_owned()), ty: u32_ty, init }, Span::UNDEFINED); // alloc: a name
        self.flag = Some(flag);
        flag
    }

    fn flag_is(&mut self, op: BinaryOperator, value: u32) -> Handle<Expression> {
        let flag = self.flag();
        let pointer = self.expr(Expression::LocalVariable(flag));
        let left = self.expr(Expression::Load { pointer });
        let right = self.expr(Expression::Literal(Literal::U32(value)));
        self.expr(Expression::Binary { op, left, right })
    }

    /// The end of the invocation: a fragment discarded, a vertex outside the clip volume.
    fn end_invocation(&mut self) {
        if let Stage::Compute { .. } = self.entry.stage {
            self.stmt(Statement::Return { value: None });
            return;
        }
        let Some(result) = self.result else { return };
        match self.entry.stage {
            Stage::Compute { .. } => unreachable!("returned above"),
            Stage::Fragment => {
                self.stmt(Statement::Kill);
                let value = self.expr(Expression::ZeroValue(result));
                self.stmt(Statement::Return { value: Some(value) });
            }
            Stage::Vertex => {
                let Out::Varyings(fields) = &self.entry.out else { return };
                let mut components = Vec::with_capacity(fields.len()); // alloc: the output's members
                for field in fields {
                    let value = match field.kind {
                        FieldKind::Position => {
                            let vec4 = self.value_ty(ValueTy::Vec4);
                            let mut parts = Vec::with_capacity(4); // alloc: a vector's parts
                            for v in [2.0, 2.0, 2.0, 1.0] {
                                parts.push(self.expr(Expression::Literal(Literal::F32(v))));
                            }
                            self.expr(Expression::Compose { ty: vec4, components: parts })
                        }
                        FieldKind::Value { ty, .. } => {
                            let ty = self.value_ty(ty);
                            self.expr(Expression::ZeroValue(ty))
                        }
                    };
                    components.push(value);
                }
                let value = self.expr(Expression::Compose { ty: result, components });
                self.stmt(Statement::Return { value: Some(value) });
            }
        }
    }

    fn run_node(&mut self, node: u32) {
        let kind = self.nodes[node as usize].kind.clone(); // clone: a small node
        match kind {
            NodeKind::End => {}
            NodeKind::Prologue => {
                if let Err(refused) = self.prologue() {
                    let item = self.entry_item();
                    self.refuse_in(&item, refused);
                }
            }
            NodeKind::Block { frame, block } => {
                let body = self.frames[frame].body;
                let data = &body.basic_blocks[block];
                for (si, statement) in data.statements.iter().enumerate() {
                    if let Err(refused) = self.statement(frame, block, si, &statement.kind) {
                        self.refuse(frame, refused);
                    }
                }
                if let Err(refused) = self.terminator_effect(frame, block) {
                    self.refuse(frame, refused);
                }
            }
        }
    }

    fn exit_node(&mut self, node: u32) {
        let kind = self.nodes[node as usize].kind.clone(); // clone: a small node
        match kind {
            NodeKind::End => self.end_invocation(),
            NodeKind::Block { frame, .. } if frame == 0 => {
                if let Err(refused) = self.entry_return() {
                    let item = self.entry_item();
                    self.refuse_in(&item, refused);
                }
            }
            // A diverging callee's block with no successor, or a node the graph left without one.
            _ => self.end_invocation(),
        }
    }

    fn prologue(&mut self) -> Result<(), Refused> {
        let params = self.frames[0].body.param_locals.clone(); // clone: a few locals
        let inputs = self.inputs.clone(); // clone: a few inputs
        for (local, input) in params.iter().zip(inputs) {
            if let Input::Thread { global, local: in_group, group } = input {
                let ty = self.local_ty(0, *local);
                let Some(var) = self.var(0, *local)? else { continue };
                let value = self.thread_in(ty, [("global", global), ("local", in_group), ("group", group)])?;
                let pointer = self.expr(Expression::LocalVariable(var));
                self.stmt(Statement::Store { pointer, value });
                continue;
            }
            let Input::Arg { arg, io } = input else { continue };
            let ty = self.local_ty(0, *local);
            let Some(var) = self.var(0, *local)? else { continue };
            let arg_expr = self.expr(Expression::FunctionArgument(arg));
            let value = match &self.dev.module.types[io].inner {
                TypeInner::Struct { members, .. } => {
                    let names: Vec<String> = members.iter().map(|m| m.name.clone().unwrap_or_default()).collect(); // clone: names
                    self.varyings_in(arg_expr, &names, ty)?
                }
                _ => self.wrap(arg_expr, io, ty)?,
            };
            let pointer = self.expr(Expression::LocalVariable(var));
            self.stmt(Statement::Store { pointer, value });
        }
        Ok(())
    }

    /// The `gpu::Thread` value from the kernel's id arguments, each Rust field from the argument
    /// of its name.
    fn thread_in(&mut self, ty: Ty<'db>, ids: [(&str, u32); 3]) -> Result<Handle<Expression>, Refused> {
        let lowered = self.dev.lower_ty(ty)?;
        let (Some(naga_ty), Shape::Struct(map)) = (lowered.naga, lowered.shape) else {
            return Err((format!("the Thread `{ty}` is not a struct", ty = self.dev.shown(ty)), why::INTERFACE)); // alloc: an error
        };
        let names = self.field_names(ty);
        let mut members: Vec<(u32, Handle<Expression>)> = Vec::with_capacity(3); // alloc: members
        for (i, name) in names.iter().enumerate() {
            let Some(k) = map[i] else { continue };
            let Some((_, arg)) = ids.into_iter().find(|(n, _)| *n == name.as_str()) else {
                return Err((format!("Thread field `{name}`, which is no kernel id"), why::INTERFACE)); // alloc: an error
            };
            members.push((k, self.expr(Expression::FunctionArgument(arg))));
        }
        members.sort_by_key(|(k, _)| *k);
        Ok(self.expr(Expression::Compose { ty: naga_ty, components: members.into_iter().map(|(_, e)| e).collect() }))
    }

    /// The Rust varyings value from the I/O struct `arg`, each Rust field from the member of its
    /// name.
    fn varyings_in(&mut self, arg: Handle<Expression>, names: &[String], ty: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        let lowered = self.dev.lower_ty(ty)?;
        let (Some(naga_ty), Shape::Struct(map)) = (lowered.naga, lowered.shape) else {
            return Err((format!("the varyings `{ty}` are not a struct", ty = self.dev.shown(ty)), why::INTERFACE)); // alloc: an error
        };
        let fields = self.dev.field_tys(ty)?;
        let rust_names = self.field_names(ty);
        let mut members: Vec<(u32, Handle<Expression>)> = Vec::new(); // alloc: members
        for (i, field_ty) in fields.iter().enumerate() {
            let Some(k) = map[i] else { continue };
            let name = &rust_names[i];
            let Some(at) = names.iter().position(|n| n == name) else {
                return Err((format!("varyings field `{name}`, which the descriptor does not list"), why::INTERFACE)); // alloc: an error
            };
            let member = self.expr(Expression::AccessIndex { base: arg, index: at as u32 });
            let io_ty = self.io_member_ty(arg, at)?;
            members.push((k, self.wrap(member, io_ty, *field_ty)?));
        }
        members.sort_by_key(|(k, _)| *k);
        Ok(self.expr(Expression::Compose { ty: naga_ty, components: members.into_iter().map(|(_, e)| e).collect() }))
    }

    fn io_member_ty(&self, arg: Handle<Expression>, at: usize) -> Result<Handle<Type>, Refused> {
        let io = match self.func.expressions[arg] {
            Expression::FunctionArgument(i) => self.func.arguments[i as usize].ty,
            _ => self.result.expect("the result is set"),
        };
        match &self.dev.module.types[io].inner {
            TypeInner::Struct { members, .. } => Ok(members[at].ty),
            _ => Err(("an I/O struct that is not a struct".to_owned(), why::INTERFACE)), // alloc: an error
        }
    }

    fn field_names(&self, ty: Ty<'db>) -> Vec<String> {
        match ty.kind() {
            TyKind::Adt(def, _) => match def.def_id() {
                AdtId::StructId(id) => VariantId::StructId(id)
                    .fields(self.dev.db)
                    .fields()
                    .iter()
                    .map(|(_, f)| f.name.as_str().to_owned()) // alloc: names
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// `value`, of the I/O type `io`, as a Rust `ty` that is it or newtypes of it.
    fn wrap(&mut self, value: Handle<Expression>, io: Handle<Type>, ty: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        let lowered = self.dev.lower_ty(ty)?;
        if lowered.naga == Some(io) {
            return Ok(value);
        }
        match (lowered.naga, &lowered.shape) {
            (Some(naga_ty), Shape::Struct(map)) if map.iter().filter(|m| m.is_some()).count() == 1 => {
                let i = map.iter().position(|m| m.is_some()).expect("one member");
                let inner = self.dev.field_tys(ty)?[i];
                let inner = self.wrap(value, io, inner)?;
                Ok(self.expr(Expression::Compose { ty: naga_ty, components: vec![inner] })) // alloc: one member
            }
            _ => Err((format!("an interface value of `{ty}`, which is not its descriptor's type or a newtype of it", ty = self.dev.shown(ty)), why::INTERFACE)), // alloc: an error
        }
    }

    /// The I/O value of a Rust `ty` that is `io` or newtypes of it.
    fn unwrap(&mut self, value: Handle<Expression>, io: Handle<Type>, ty: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        let lowered = self.dev.lower_ty(ty)?;
        if lowered.naga == Some(io) {
            return Ok(value);
        }
        match &lowered.shape {
            Shape::Struct(map) if map.iter().filter(|m| m.is_some()).count() == 1 => {
                let i = map.iter().position(|m| m.is_some()).expect("one member");
                let inner_ty = self.dev.field_tys(ty)?[i];
                let inner = self.expr(Expression::AccessIndex { base: value, index: 0 });
                self.unwrap(inner, io, inner_ty)
            }
            _ => Err((format!("an interface value of `{ty}`, which is not its descriptor's type or a newtype of it", ty = self.dev.shown(ty)), why::INTERFACE)), // alloc: an error
        }
    }

    fn entry_return(&mut self) -> Result<(), Refused> {
        if self.entry.out == Out::Nothing {
            self.stmt(Statement::Return { value: None });
            return Ok(());
        }
        let result = self.result.expect("the interface is set");
        let ret = m::return_slot();
        let ty = self.local_ty(0, ret);
        let var = self.var(0, ret)?.ok_or_else(|| ("an entry returning a value of no size".to_owned(), why::INTERFACE))?; // alloc: an error
        let pointer = self.expr(Expression::LocalVariable(var));
        let value = self.expr(Expression::Load { pointer });
        let out = match &self.entry.out {
            Out::Nothing => unreachable!("returned above"),
            Out::Color => value,
            Out::Varyings(fields) => {
                let lowered = self.dev.lower_ty(ty)?;
                let Shape::Struct(map) = lowered.shape else {
                    return Err((format!("varyings `{ty}` that are not a struct", ty = self.dev.shown(ty)), why::INTERFACE)); // alloc: an error
                };
                let rust_names = self.field_names(ty);
                let field_tys = self.dev.field_tys(ty)?;
                let mut components = Vec::with_capacity(fields.len()); // alloc: the output's members
                for (at, field) in fields.iter().enumerate() {
                    let Some(i) = rust_names.iter().position(|n| *n == field.name) else {
                        return Err((format!("varyings field `{}`, which the struct does not have", field.name), why::INTERFACE)); // alloc: an error
                    };
                    let Some(k) = map[i] else {
                        return Err((format!("varyings field `{}` of no size", field.name), why::INTERFACE)); // alloc: an error
                    };
                    let member = self.expr(Expression::AccessIndex { base: value, index: k });
                    let io_ty = match &self.dev.module.types[result].inner {
                        TypeInner::Struct { members, .. } => members[at].ty,
                        _ => unreachable!("varyings out are a struct"),
                    };
                    components.push(self.unwrap(member, io_ty, field_tys[i])?);
                }
                self.expr(Expression::Compose { ty: result, components })
            }
        };
        self.stmt(Statement::Return { value: Some(out) });
        Ok(())
    }

    /// A statement's effect.
    fn statement(&mut self, frame: usize, block: m::BasicBlockId, si: usize, kind: &m::StatementKind) -> Result<(), Refused> {
        match kind {
            m::StatementKind::Assign(place, rvalue) => {
                if place.projection.as_slice().is_empty() && self.is_ref(self.local_ty(frame, place.local)) {
                    // Bound when the graph was built; write the indices it snapshots.
                    let writes = self.snapshot_writes.get(&(frame, block, si)).cloned().unwrap_or_default(); // clone: a few writes
                    for (snap, index) in writes {
                        let value = self.load_local(frame, index)?;
                        let pointer = self.expr(Expression::LocalVariable(snap));
                        self.stmt(Statement::Store { pointer, value });
                    }
                    return Ok(());
                }
                let dest_ty = self.place_ty(frame, place);
                let Some(value) = self.rvalue(frame, rvalue, dest_ty)? else { return Ok(()) };
                match self.place_ptr(frame, place)? {
                    Ptr::Pointer(pointer, _) => self.stmt(Statement::Store { pointer, value }),
                    Ptr::Zst => {}
                    Ptr::Handle(_) => return Err(("a write to a texture or sampler".to_owned(), why::OPERATIONS)), // alloc: an error
                }
                Ok(())
            }
            m::StatementKind::FakeRead(_)
            | m::StatementKind::Nop
            | m::StatementKind::StorageLive(_)
            | m::StatementKind::StorageDead(_)
            | m::StatementKind::Deinit(_) => Ok(()),
            m::StatementKind::InlineAsm(_) => Err(("inline assembly".to_owned(), why::OPERATIONS)), // alloc: an error
        }
    }

    fn load_local(&mut self, frame: usize, local: m::LocalId) -> Result<Handle<Expression>, Refused> {
        let var = self.var(frame, local)?.ok_or_else(|| ("an index of no size".to_owned(), why::TYPES))?; // alloc: an error
        let pointer = self.expr(Expression::LocalVariable(var));
        Ok(self.expr(Expression::Load { pointer }))
    }

    /// The pointer a value place names.
    fn place_ptr(&mut self, frame: usize, place: &m::StoredPlace) -> Result<Ptr<'db>, Refused> {
        let elems = place.projection.as_slice();
        let (mut ptr, ty, rest) = match elems.first() {
            Some(m::ProjectionElem::Deref) => {
                let sym = self.sym_of_ref_value(frame, &m::Place::from(place.local).store())?;
                // alloc: an error
                let inner = pointee(self.local_ty(frame, place.local)).ok_or_else(|| ("a deref of a non-reference".to_owned(), why::POINTERS))?;
                let Some(ptr) = self.sym_ptr(&sym) else { return Ok(Ptr::Zst) };
                (ptr, self.dev.normalize(inner), &elems[1..])
            }
            _ => {
                let ty = self.local_ty(frame, place.local);
                if self.is_ref(ty) {
                    return Err(("a reference used as a value".to_owned(), why::POINTERS)); // alloc: an error
                }
                match self.var(frame, place.local)? {
                    Some(var) => (self.expr(Expression::LocalVariable(var)), ty, elems),
                    None => return Ok(Ptr::Zst),
                }
            }
        };
        let mut pty = m::PlaceTy::from_ty(ty);
        for elem in rest {
            let next = m::PlaceTy::projection_ty(pty, &self.dev.infcx, elem, self.dev.env.param_env);
            if self.dev.lower_ty(self.dev.normalize(next.ty))?.naga.is_none() {
                // A field or element of no size: nothing to point at.
                return Ok(Ptr::Zst);
            }
            ptr = match elem {
                m::ProjectionElem::Field(field) => {
                    let index = self.member(pty.ty, pty.variant_id, field.0 as usize)?;
                    self.expr(Expression::AccessIndex { base: ptr, index })
                }
                m::ProjectionElem::Downcast(_) => ptr,
                m::ProjectionElem::ConstantIndex { offset, from_end: false, .. } => {
                    self.expr(Expression::AccessIndex { base: ptr, index: *offset as u32 })
                }
                m::ProjectionElem::Index(index) => {
                    let index = self.load_local(frame, *index)?;
                    self.expr(Expression::Access { base: ptr, index })
                }
                m::ProjectionElem::Deref => return Err(("a deref of a reference held in a field".to_owned(), why::POINTERS)), // alloc: an error
                other => return Err((format!("the projection `{other:?}`"), why::OPERATIONS)), // alloc: an error
            };
            pty = next;
            pty.ty = self.dev.normalize(pty.ty);
        }
        let lowered = self.dev.lower_ty(pty.ty)?;
        Ok(match lowered.shape {
            Shape::Zst => Ptr::Zst,
            Shape::Texture | Shape::Sampler | Shape::Tlas => Ptr::Handle(ptr),
            _ => Ptr::Pointer(ptr, pty.ty),
        })
    }

    /// The pointer a symbolic place is; `None` for a place of no size.
    fn sym_ptr(&mut self, sym: &Sym) -> Option<Handle<Expression>> {
        let mut ptr = match sym.root {
            Root::Var(v) => self.expr(Expression::LocalVariable(v)),
            Root::Global(g) => self.expr(Expression::GlobalVariable(g)),
            Root::Zst => return None,
        };
        for step in &sym.steps {
            ptr = match *step {
                Step::Member(index) => self.expr(Expression::AccessIndex { base: ptr, index }),
                Step::Index(snap) => {
                    let pointer = self.expr(Expression::LocalVariable(snap));
                    let index = self.expr(Expression::Load { pointer });
                    self.expr(Expression::Access { base: ptr, index })
                }
            };
        }
        Some(ptr)
    }

    /// The length of the slice a symbolic place names: the unsized array's, or a runtime-sized
    /// array's `arrayLength`.
    fn sym_len(&mut self, sym: &Sym) -> Result<Handle<Expression>, Refused> {
        if let Some(len) = sym.len {
            return Ok(self.expr(Expression::Literal(Literal::U32(len))));
        }
        match self.sym_ptr(sym) {
            Some(pointer) => Ok(self.expr(Expression::ArrayLength(pointer))),
            None => Ok(self.expr(Expression::Literal(Literal::U32(0)))),
        }
    }

    /// An operand's value; `None` for one of no size.
    fn operand(&mut self, frame: usize, op: &m::Operand) -> Result<Option<Handle<Expression>>, Refused> {
        match &op.kind {
            m::OperandKind::Copy(place) | m::OperandKind::Move(place) => match self.place_ptr(frame, place)? {
                Ptr::Pointer(pointer, _) => Ok(Some(self.expr(Expression::Load { pointer }))),
                Ptr::Handle(h) => Ok(Some(h)),
                Ptr::Zst => Ok(None),
            },
            m::OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if op.fn_def().is_some() {
                    return Ok(None);
                }
                let lowered = self.dev.lower_ty(ty)?;
                let Some(_) = lowered.naga else { return Ok(None) };
                let bits = match konst.as_ref().kind() {
                    ConstKind::Value(value) => value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked()),
                    _ => None,
                };
                let Some(bits) = bits else {
                    return Err((format!("a constant of type `{ty}`", ty = self.dev.shown(ty)), why::OPERATIONS)); // alloc: an error
                };
                let Shape::Scalar(scalar) = lowered.shape else {
                    return Err((format!("a constant of type `{ty}`", ty = self.dev.shown(ty)), why::OPERATIONS)); // alloc: an error
                };
                let lit = match scalar.kind {
                    ScalarKind::Bool => Literal::Bool(bits != 0),
                    ScalarKind::Sint => Literal::I32(bits as u32 as i32),
                    ScalarKind::Uint => Literal::U32(bits as u32),
                    ScalarKind::Float => Literal::F32(f32::from_bits(bits as u32)),
                    _ => return Err((format!("a constant of type `{ty}`", ty = self.dev.shown(ty)), why::OPERATIONS)), // alloc: an error
                };
                Ok(Some(self.expr(Expression::Literal(lit))))
            }
            m::OperandKind::Allocation { allocation } => {
                if op.fn_def().is_some() {
                    return Ok(None);
                }
                if self.static_of(op).is_some() {
                    return Err(self.reference_constant(op, "a constant that points into a static"));
                }
                let allocation = allocation.as_ref();
                if !matches!(allocation.memory_map, MemoryMap::Empty) && !allocation.memory_map.value_pointers().is_empty() {
                    return Err((format!("a constant of `{}` that holds references", self.dev.shown(allocation.ty)), why::POINTERS)); // alloc: an error
                }
                match self.dev.constant(allocation.ty, &allocation.memory)? {
                    Some(c) => Ok(Some(self.expr(Expression::Constant(c)))),
                    None => Ok(None),
                }
            }
            m::OperandKind::Static(_) => Err(self.reference_constant(op, "a read of a static")),
        }
    }

    /// The static a reference constant points into: a static's address (`OperandKind::Static`),
    /// or a constant reference whose address the evaluation gave to a static's memory.
    fn static_of(&self, op: &m::Operand) -> Option<StaticId> {
        match &op.kind {
            m::OperandKind::Static(id) => Some(*id),
            m::OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                if !self.is_ref(allocation.ty) {
                    return None;
                }
                let dl = self.dev.db.target_data_layout(self.dev.env.krate).ok()?;
                let word = allocation.memory.get(..dl.pointer_size().bytes_usize())?;
                let mut bytes = [0u8; 8];
                match dl.endian {
                    Endian::Little => bytes[..word.len()].copy_from_slice(word),
                    Endian::Big => {
                        for (i, b) in word.iter().rev().enumerate() {
                            bytes[i] = *b;
                        }
                    }
                }
                let addr = usize::try_from(u64::from_le_bytes(bytes)).ok()?;
                allocation.memory_map.static_at(addr).map(|(id, _)| id)
            }
            m::OperandKind::Copy(_) | m::OperandKind::Move(_) | m::OperandKind::Constant { .. } => None,
        }
    }

    /// The refusal of a reference that is a constant: one into a static is a read of that
    /// static, refused under the statics rule and named by its path; any other is `what`,
    /// under the references rule.
    fn reference_constant(&self, op: &m::Operand, what: &str) -> Refused {
        match self.static_of(op) {
            Some(statik) => {
                let path = blaze_items::key::path(self.dev.db, blaze_items::ItemId::Static(statik));
                (format!("a read of the static `{path}`"), why::STATICS) // alloc: an error
            }
            None => (what.to_owned(), why::POINTERS), // alloc: an error
        }
    }

    fn operand_ty(&self, frame: usize, op: &m::Operand) -> Option<Ty<'db>> {
        match &op.kind {
            m::OperandKind::Copy(place) | m::OperandKind::Move(place) => Some(self.place_ty(frame, place)),
            m::OperandKind::Constant { ty, .. } => Some(self.dev.normalize(ty.as_ref())),
            m::OperandKind::Allocation { allocation } => Some(self.dev.normalize(allocation.as_ref().ty)),
            m::OperandKind::Static(_) => None,
        }
    }

    fn scalar_of(&mut self, ty: Ty<'db>) -> Result<Scalar, Refused> {
        match self.dev.lower_ty(ty)?.shape {
            Shape::Scalar(s) => Ok(s),
            _ => Err((format!("an operation on `{ty}`, which is not a scalar", ty = self.dev.shown(ty)), why::OPERATIONS)), // alloc: an error
        }
    }

    fn need(&self, value: Option<Handle<Expression>>) -> Result<Handle<Expression>, Refused> {
        value.ok_or_else(|| ("an operation on a value of no size".to_owned(), why::OPERATIONS)) // alloc: an error
    }

    fn rvalue(&mut self, frame: usize, rvalue: &m::Rvalue, dest_ty: Ty<'db>) -> Result<Option<Handle<Expression>>, Refused> {
        Ok(match rvalue {
            m::Rvalue::Use(op) => self.operand(frame, op)?,
            m::Rvalue::CopyForDeref(place) => match self.place_ptr(frame, place)? {
                Ptr::Pointer(pointer, _) => Some(self.expr(Expression::Load { pointer })),
                Ptr::Handle(h) => Some(h),
                Ptr::Zst => None,
            },
            m::Rvalue::Ref(..) => return Err(("a reference stored as a value".to_owned(), why::POINTERS)), // alloc: an error
            m::Rvalue::Len(place) => {
                let ty = self.place_ty(frame, place);
                match ty.kind() {
                    TyKind::Array(..) => match self.dev.lower_ty(ty)?.shape {
                        Shape::Array { len } => Some(self.expr(Expression::Literal(Literal::U32(len)))),
                        _ => Some(self.expr(Expression::Literal(Literal::U32(0)))),
                    },
                    TyKind::Slice(_) => {
                        let [m::ProjectionElem::Deref] = place.projection.as_slice() else {
                            return Err(("the length of a slice not reached through a reference".to_owned(), why::POINTERS)); // alloc: an error
                        };
                        let sym = self.sym_of_ref_value(frame, &m::Place::from(place.local).store())?;
                        Some(self.sym_len(&sym)?)
                    }
                    _ => return Err((format!("the length of `{ty}`", ty = self.dev.shown(ty)), why::OPERATIONS)), // alloc: an error
                }
            }
            m::Rvalue::Repeat(op, _) => {
                let lowered = self.dev.lower_ty(dest_ty)?;
                let (Some(naga_ty), Shape::Array { len }) = (lowered.naga, lowered.shape) else { return Ok(None) };
                let value = self.operand(frame, op)?;
                let value = self.need(value)?;
                Some(self.expr(Expression::Compose { ty: naga_ty, components: vec![value; len as usize] })) // alloc: the elements
            }
            m::Rvalue::Cast(kind, op, to) => {
                let from_ty = self.operand_ty(frame, op).ok_or_else(|| ("a cast of a static".to_owned(), why::STATICS))?; // alloc: an error
                let value = self.operand(frame, op)?;
                let value = self.need(value)?;
                Some(self.cast(kind, value, from_ty, self.dev.normalize(to.as_ref()))?)
            }
            m::Rvalue::BinaryOp(op, a, b) | m::Rvalue::CheckedBinaryOp(op, a, b) => {
                let ty = self.operand_ty(frame, a).ok_or_else(|| ("an operation on a static".to_owned(), why::STATICS))?; // alloc: an error
                let rhs_ty = self.operand_ty(frame, b).ok_or_else(|| ("an operation on a static".to_owned(), why::STATICS))?; // alloc: an error
                let left = self.operand(frame, a)?;
                let left = self.need(left)?;
                let right = self.operand(frame, b)?;
                let right = self.need(right)?;
                let checked = matches!(rvalue, m::Rvalue::CheckedBinaryOp(..));
                Some(self.binary(op, left, right, ty, rhs_ty, dest_ty, checked)?)
            }
            m::Rvalue::UnaryOp(op, a) => {
                let ty = self.operand_ty(frame, a).ok_or_else(|| ("an operation on a static".to_owned(), why::STATICS))?; // alloc: an error
                let value = self.operand(frame, a)?;
                let expr = self.need(value)?;
                let scalar = self.scalar_of(ty)?;
                let op = match (op, scalar.kind) {
                    (m::UnOp::Not, ScalarKind::Bool) => UnaryOperator::LogicalNot,
                    (m::UnOp::Not, _) => UnaryOperator::BitwiseNot,
                    (m::UnOp::Neg, _) => UnaryOperator::Negate,
                };
                Some(self.expr(Expression::Unary { op, expr }))
            }
            m::Rvalue::Aggregate(kind, ops) => {
                match kind {
                    m::AggregateKind::Tuple(_) | m::AggregateKind::Array(_) => {}
                    m::AggregateKind::Adt(VariantId::StructId(_), _) => {}
                    m::AggregateKind::Adt(VariantId::EnumVariantId(variant), _) => return self.enum_value(frame, *variant, ops, dest_ty),
                    _ => return Err(("a union or closure value".to_owned(), why::TYPES)), // alloc: an error
                }
                let lowered = self.dev.lower_ty(dest_ty)?;
                let Some(naga_ty) = lowered.naga else { return Ok(None) };
                let mut values = Vec::with_capacity(ops.len()); // alloc: an aggregate's fields
                for op in ops.iter() {
                    values.push(self.operand(frame, op)?);
                }
                let components: Vec<Handle<Expression>> = match &lowered.shape {
                    Shape::Struct(map) => {
                        let mut members: Vec<(u32, Handle<Expression>)> = Vec::new(); // alloc: members
                        for (i, v) in values.into_iter().enumerate() {
                            if let (Some(k), Some(v)) = (map.get(i).copied().flatten(), v) {
                                members.push((k, v));
                            }
                        }
                        members.sort_by_key(|(k, _)| *k);
                        members.into_iter().map(|(_, v)| v).collect()
                    }
                    Shape::Vector | Shape::Array { .. } => values.into_iter().map(|v| self.need(v)).collect::<Result<_, _>>()?,
                    _ => return Err((format!("a value of `{dest_ty}` built from parts", dest_ty = self.dev.shown(dest_ty)), why::TYPES)), // alloc: an error
                };
                Some(self.expr(Expression::Compose { ty: naga_ty, components }))
            }
            m::Rvalue::Discriminant(place) => {
                let ty = self.place_ty(frame, place);
                let dest = self.scalar_of(dest_ty)?;
                match self.dev.lower_ty(ty)?.shape {
                    Shape::Enum(_) => {
                        let Ptr::Pointer(pointer, _) = self.place_ptr(frame, place)? else {
                            return Err((format!("the discriminant of `{ty}`, which has no value", ty = self.dev.shown(ty)), why::ENUMS)); // alloc: an error
                        };
                        let tag = self.expr(Expression::AccessIndex { base: pointer, index: 0 });
                        let tag = self.expr(Expression::Load { pointer: tag });
                        match dest.kind {
                            ScalarKind::Sint => Some(tag),
                            kind => Some(self.expr(Expression::As { expr: tag, kind, convert: None })),
                        }
                    }
                    // Every other type's discriminant is 0, Rust's rule.
                    _ => Some(self.expr(Expression::Literal(match dest.kind {
                        ScalarKind::Sint => Literal::I32(0),
                        _ => Literal::U32(0),
                    }))),
                }
            }
            m::Rvalue::ThreadLocalRef(never) | m::Rvalue::AddressOf(never) | m::Rvalue::NullaryOp(never) => match *never {},
        })
    }

    /// An enum value of `variant` from its fields: the discriminant, the variant's members, and
    /// every other variant's members zero.
    fn enum_value(&mut self, frame: usize, variant: EnumVariantId, ops: &[m::Operand], dest_ty: Ty<'db>) -> Result<Option<Handle<Expression>>, Refused> {
        let lowered = self.dev.lower_ty(dest_ty)?;
        let (Some(naga_ty), Shape::Enum(variants)) = (lowered.naga, lowered.shape) else { return Ok(None) };
        let Some(this) = variants.iter().find(|v| v.id == variant) else {
            return Err((format!("a variant that is not of `{dest_ty}`", dest_ty = self.dev.shown(dest_ty)), why::ENUMS)); // alloc: an error
        };
        let TypeInner::Struct { members, .. } = &self.dev.module.types[naga_ty].inner else { unreachable!("an enum is a struct") };
        let member_tys: Vec<Handle<Type>> = members.iter().map(|m| m.ty).collect(); // alloc: member types
        let mut components: Vec<Option<Handle<Expression>>> = vec![None; member_tys.len()]; // alloc: members
        components[0] = Some(self.expr(Expression::Literal(Literal::I32(this.discr as i32))));
        for (i, op) in ops.iter().enumerate() {
            let value = self.operand(frame, op)?;
            if let (Some(k), Some(value)) = (this.members.get(i).copied().flatten(), value) {
                components[k as usize] = Some(value);
            }
        }
        let mut all = Vec::with_capacity(components.len()); // alloc: members
        for (c, ty) in components.into_iter().zip(member_tys) {
            all.push(match c {
                Some(c) => c,
                None => self.expr(Expression::ZeroValue(ty)),
            });
        }
        Ok(Some(self.expr(Expression::Compose { ty: naga_ty, components: all })))
    }

    fn cast(&mut self, kind: &m::CastKind, value: Handle<Expression>, from: Ty<'db>, to: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        if matches!(kind, m::CastKind::Transmute) {
            return self.bitcast(value, from, to);
        }
        let from_s = self.scalar_of(from)?;
        let to_s = self.scalar_of(to)?;
        Ok(match kind {
            m::CastKind::IntToInt => {
                if from_s == to_s {
                    value
                } else if from_s.kind == ScalarKind::Bool {
                    self.expr(Expression::As { expr: value, kind: to_s.kind, convert: Some(4) })
                } else {
                    // Both 32 bits: Rust's `as` between them keeps the bits.
                    self.expr(Expression::As { expr: value, kind: to_s.kind, convert: None })
                }
            }
            m::CastKind::IntToFloat => self.expr(Expression::As { expr: value, kind: ScalarKind::Float, convert: Some(4) }),
            m::CastKind::FloatToFloat if from_s == to_s => value,
            m::CastKind::FloatToInt => self.float_to_int(value, to_s)?,
            other => return Err((format!("a `{other:?}` cast from `{from}` to `{to}`", from = self.dev.shown(from), to = self.dev.shown(to)), why::OPERATIONS)), // alloc: an error
        })
    }

    /// `transmute`: the same bits as another type of the same size. The same naga type is the
    /// value itself; two 32-bit scalars are naga's bitcast; anything else is refused.
    fn bitcast(&mut self, value: Handle<Expression>, from: Ty<'db>, to: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        let (from_l, to_l) = (self.dev.lower_ty(from)?, self.dev.lower_ty(to)?);
        if from_l.naga == to_l.naga {
            return Ok(value);
        }
        match (from_l.shape, to_l.shape) {
            (Shape::Scalar(a), Shape::Scalar(b)) if a.width == 4 && b.width == 4 => {
                Ok(self.expr(Expression::As { expr: value, kind: b.kind, convert: None }))
            }
            _ => Err((format!("a transmute from `{from}` to `{to}`", from = self.dev.shown(from), to = self.dev.shown(to)), why::OPERATIONS)), // alloc: an error
        }
    }

    /// Rust's saturating float-to-int `as`: NaN is 0, and a value past either end the end.
    fn float_to_int(&mut self, x: Handle<Expression>, to: Scalar) -> Result<Handle<Expression>, Refused> {
        // The ends as floats that convert exactly, and where the integer's top is.
        let (lo, hi, top, max) = match to.kind {
            ScalarKind::Sint => (-2147483648.0f32, 2147483520.0f32, 2147483648.0f32, Literal::I32(i32::MAX)),
            ScalarKind::Uint => (0.0f32, 4294967040.0f32, 4294967296.0f32, Literal::U32(u32::MAX)),
            _ => return Err(("a float cast to a non-integer".to_owned(), why::OPERATIONS)), // alloc: an error
        };
        let lo = self.expr(Expression::Literal(Literal::F32(lo)));
        let hi = self.expr(Expression::Literal(Literal::F32(hi)));
        let clamped = self.expr(Expression::Math { fun: MathFunction::Clamp, arg: x, arg1: Some(lo), arg2: Some(hi), arg3: None });
        let converted = self.expr(Expression::As { expr: clamped, kind: to.kind, convert: Some(4) });
        let top = self.expr(Expression::Literal(Literal::F32(top)));
        let past = self.expr(Expression::Binary { op: BinaryOperator::GreaterEqual, left: x, right: top });
        let max = self.expr(Expression::Literal(max));
        let saturated = self.expr(Expression::Select { condition: past, accept: max, reject: converted });
        let nan = self.expr(Expression::Binary { op: BinaryOperator::NotEqual, left: x, right: x });
        let zero_ty = self.dev.module.types.insert(Type { name: None, inner: TypeInner::Scalar(to) }, Span::UNDEFINED);
        let zero = self.expr(Expression::ZeroValue(zero_ty));
        Ok(self.expr(Expression::Select { condition: nan, accept: zero, reject: saturated }))
    }

    #[allow(clippy::too_many_arguments)]
    fn binary(
        &mut self,
        op: &m::BinOp,
        left: Handle<Expression>,
        right: Handle<Expression>,
        ty: Ty<'db>,
        rhs_ty: Ty<'db>,
        dest_ty: Ty<'db>,
        checked: bool,
    ) -> Result<Handle<Expression>, Refused> {
        let scalar = self.scalar_of(ty)?;
        let overflowing = checked
            || matches!(op, m::BinOp::AddWithOverflow | m::BinOp::SubWithOverflow | m::BinOp::MulWithOverflow);
        let base = match op {
            m::BinOp::Add | m::BinOp::AddWithOverflow => BinaryOperator::Add,
            m::BinOp::Sub | m::BinOp::SubWithOverflow => BinaryOperator::Subtract,
            m::BinOp::Mul | m::BinOp::MulWithOverflow => BinaryOperator::Multiply,
            m::BinOp::Div => BinaryOperator::Divide,
            m::BinOp::Rem => BinaryOperator::Modulo,
            m::BinOp::BitAnd if scalar.kind == ScalarKind::Bool => BinaryOperator::LogicalAnd,
            m::BinOp::BitOr if scalar.kind == ScalarKind::Bool => BinaryOperator::LogicalOr,
            m::BinOp::BitXor if scalar.kind == ScalarKind::Bool => BinaryOperator::NotEqual,
            m::BinOp::BitAnd => BinaryOperator::And,
            m::BinOp::BitOr => BinaryOperator::InclusiveOr,
            m::BinOp::BitXor => BinaryOperator::ExclusiveOr,
            m::BinOp::Shl | m::BinOp::Shr => {
                // Rust's shift past the width is checked before (an `assert`) or masked; the
                // device's is undefined, so the amount is masked to the width.
                let rhs_s = self.scalar_of(rhs_ty)?;
                let amount = if rhs_s.kind == ScalarKind::Uint { right } else { self.expr(Expression::As { expr: right, kind: ScalarKind::Uint, convert: None }) };
                let mask = self.expr(Expression::Literal(Literal::U32(31)));
                let amount = self.expr(Expression::Binary { op: BinaryOperator::And, left: amount, right: mask });
                let op = if matches!(op, m::BinOp::Shl) { BinaryOperator::ShiftLeft } else { BinaryOperator::ShiftRight };
                return Ok(self.expr(Expression::Binary { op, left, right: amount }));
            }
            m::BinOp::Eq => BinaryOperator::Equal,
            m::BinOp::Ne => BinaryOperator::NotEqual,
            m::BinOp::Lt => BinaryOperator::Less,
            m::BinOp::Le => BinaryOperator::LessEqual,
            m::BinOp::Gt => BinaryOperator::Greater,
            m::BinOp::Ge => BinaryOperator::GreaterEqual,
            m::BinOp::Offset => return Err(("pointer `offset`".to_owned(), why::POINTERS)), // alloc: an error
        };
        let value = self.expr(Expression::Binary { op: base, left, right });
        if !overflowing {
            return Ok(value);
        }
        // `(value, overflowed)`, the tuple runtime MIR asserts on.
        let overflowed = self.overflowed(base, left, right, value, scalar)?;
        let tuple = self.dev.lower_ty(dest_ty)?;
        let (Some(tuple_ty), Shape::Struct(map)) = (tuple.naga, tuple.shape) else {
            return Err(("an overflowing operation not into a tuple".to_owned(), why::OPERATIONS)); // alloc: an error
        };
        let mut parts = [(map[0], value), (map[1], overflowed)];
        parts.sort_by_key(|(k, _)| *k);
        Ok(self.expr(Expression::Compose { ty: tuple_ty, components: parts.iter().map(|(_, e)| e.clone()).collect() }))
    }

    /// Whether `value = left op right` overflowed, for 32-bit integers.
    fn overflowed(&mut self, op: BinaryOperator, a: Handle<Expression>, b: Handle<Expression>, r: Handle<Expression>, scalar: Scalar) -> Result<Handle<Expression>, Refused> {
        let bin = |cx: &mut Self, op, left, right| cx.expr(Expression::Binary { op, left, right });
        let zero = match scalar.kind {
            ScalarKind::Sint => self.expr(Expression::Literal(Literal::I32(0))),
            ScalarKind::Uint => self.expr(Expression::Literal(Literal::U32(0))),
            _ => return Err(("an overflow check of a non-integer".to_owned(), why::OPERATIONS)), // alloc: an error
        };
        let signed = scalar.kind == ScalarKind::Sint;
        Ok(match (op, signed) {
            // Unsigned: the sum wrapped below a part; the difference took more than there was.
            (BinaryOperator::Add, false) => bin(self, BinaryOperator::Less, r, a),
            (BinaryOperator::Subtract, false) => bin(self, BinaryOperator::Less, a, b),
            // Signed: the result's sign differs from both parts' (add), or from a's where b's
            // differs from a's (subtract).
            (BinaryOperator::Add, true) => {
                let x = bin(self, BinaryOperator::ExclusiveOr, a, r);
                let y = bin(self, BinaryOperator::ExclusiveOr, b, r);
                let both = bin(self, BinaryOperator::And, x, y);
                bin(self, BinaryOperator::Less, both, zero)
            }
            (BinaryOperator::Subtract, true) => {
                let x = bin(self, BinaryOperator::ExclusiveOr, a, b);
                let y = bin(self, BinaryOperator::ExclusiveOr, a, r);
                let both = bin(self, BinaryOperator::And, x, y);
                bin(self, BinaryOperator::Less, both, zero)
            }
            // Multiply: a nonzero `a` whose product divided back is not `b` (and, signed, the one
            // product past i32::MIN that dividing can't see: -1 times MIN).
            (BinaryOperator::Multiply, _) => {
                let nonzero = bin(self, BinaryOperator::NotEqual, a, zero);
                let one = match scalar.kind {
                    ScalarKind::Sint => self.expr(Expression::Literal(Literal::I32(1))),
                    _ => self.expr(Expression::Literal(Literal::U32(1))),
                };
                let safe_a = self.expr(Expression::Select { condition: nonzero, accept: a, reject: one });
                let back = bin(self, BinaryOperator::Divide, r, safe_a);
                let differs = bin(self, BinaryOperator::NotEqual, back, b);
                let mut wrong = bin(self, BinaryOperator::LogicalAnd, nonzero, differs);
                if signed {
                    let minus_one = self.expr(Expression::Literal(Literal::I32(-1)));
                    let min = self.expr(Expression::Literal(Literal::I32(i32::MIN)));
                    let a_neg = bin(self, BinaryOperator::Equal, a, minus_one);
                    let b_min = bin(self, BinaryOperator::Equal, b, min);
                    let edge = bin(self, BinaryOperator::LogicalAnd, a_neg, b_min);
                    wrong = bin(self, BinaryOperator::LogicalOr, wrong, edge);
                }
                wrong
            }
            _ => return Err(("an overflow check of this operation".to_owned(), why::OPERATIONS)), // alloc: an error
        })
    }

    /// What a block's terminator does when the block runs: a call's arguments into its callee,
    /// a callee's result back to its caller, an intrinsic's value.
    fn terminator_effect(&mut self, frame: usize, block: m::BasicBlockId) -> Result<(), Refused> {
        let body = self.frames[frame].body;
        let Some(term) = &body.basic_blocks[block].terminator else { return Ok(()) };
        match &term.kind {
            m::TerminatorKind::Return => {
                let Some((caller, dest, _)) = self.frames[frame].caller.clone() else { return Ok(()) }; // clone: the caller's place
                let ret = m::return_slot();
                if self.is_ref(self.local_ty(frame, ret)) {
                    return Ok(());
                }
                let Some(var) = self.var(frame, ret)? else { return Ok(()) };
                let pointer = self.expr(Expression::LocalVariable(var));
                let value = self.expr(Expression::Load { pointer });
                if let Ptr::Pointer(pointer, _) = self.place_ptr(caller, &dest)? {
                    self.stmt(Statement::Store { pointer, value });
                }
                Ok(())
            }
            m::TerminatorKind::Call { args, destination, .. } => {
                let call = self.frames[frame].calls.get(&block).cloned().expect("decided when dooming"); // clone: a small decision
                match call {
                    Target::Inline(..) => {
                        let callee = self.callee_of(frame, block);
                        let params = self.frames[callee].body.param_locals.clone(); // clone: a few locals
                        for (param, arg) in params.iter().zip(args.iter()) {
                            if self.is_ref(self.local_ty(callee, *param)) {
                                continue;
                            }
                            let Some(value) = self.operand(frame, arg)? else { continue };
                            if let Some(var) = self.var(callee, *param)? {
                                let pointer = self.expr(Expression::LocalVariable(var));
                                self.stmt(Statement::Store { pointer, value });
                            }
                        }
                        Ok(())
                    }
                    Target::Math(fun, arity) => {
                        if args.len() != arity {
                            return Err((format!("`{fun:?}` with {} arguments", args.len()), why::OPERATIONS)); // alloc: an error
                        }
                        let mut values = Vec::with_capacity(arity); // alloc: arguments
                        for arg in args.iter() {
                            let ty = self.operand_ty(frame, arg).ok_or_else(|| ("a static argument".to_owned(), why::STATICS))?; // alloc: an error
                            self.scalar_of(ty)?;
                            let v = self.operand(frame, arg)?;
                            values.push(self.need(v)?);
                        }
                        let value = self.expr(Expression::Math { fun, arg: values[0], arg1: values.get(1).copied(), arg2: values.get(2).copied(), arg3: None });
                        self.store_to(frame, destination, value)
                    }
                    Target::False => {
                        let value = self.expr(Expression::Literal(Literal::Bool(false)));
                        self.store_to(frame, destination, value)
                    }
                    Target::CloneCopy => {
                        let [arg] = &args[..] else {
                            return Err(("a builtin `clone` without one argument".to_owned(), why::CALLS)); // alloc: an error
                        };
                        let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &arg.kind else {
                            return Err(("a builtin `clone` of a constant reference".to_owned(), why::POINTERS)); // alloc: an error
                        };
                        let sym = self.sym_of_ref_value(frame, of)?;
                        let Some(pointer) = self.sym_ptr(&sym) else { return Ok(()) };
                        let value = self.expr(Expression::Load { pointer });
                        self.store_to(frame, destination, value)
                    }
                    Target::Core(op) => {
                        let dest_ty = self.place_ty(frame, destination);
                        let Some(value) = self.core_call(frame, &op, args, dest_ty)? else { return Ok(()) };
                        self.store_to(frame, destination, value)
                    }
                    Target::Nothing | Target::Panic | Target::Refused(..) | Target::Device(DeviceFn::Discard) => Ok(()),
                    Target::Device(device) => {
                        let dest_ty = self.place_ty(frame, destination);
                        let Some(value) = self.device_call(frame, device, args, dest_ty)? else { return Ok(()) };
                        self.store_to(frame, destination, value)
                    }
                }
            }
            _ => Ok(()),
        }
    }

    /// The frame a block's inlined call made.
    fn callee_of(&self, frame: usize, block: m::BasicBlockId) -> usize {
        *self.callees.get(&(frame, block)).expect("an inlined call's frame is made when the graph is built")
    }

    fn store_to(&mut self, frame: usize, dest: &m::StoredPlace, value: Handle<Expression>) -> Result<(), Refused> {
        if let Ptr::Pointer(pointer, _) = self.place_ptr(frame, dest)? {
            self.stmt(Statement::Store { pointer, value });
        }
        Ok(())
    }

    /// A core intrinsic's value; `None` for one of no size.
    fn core_call(&mut self, frame: usize, op: &CoreOp, args: &[m::Operand], dest_ty: Ty<'db>) -> Result<Option<Handle<Expression>>, Refused> {
        let arg_ty = |cx: &Self, i: usize| -> Result<Ty<'db>, Refused> {
            let arg = args.get(i).ok_or_else(|| (format!("`{op:?}` with too few arguments"), why::OPERATIONS))?; // alloc: an error
            cx.operand_ty(frame, arg).ok_or_else(|| ("a static argument".to_owned(), why::STATICS)) // alloc: an error
        };
        let value = |cx: &mut Self, i: usize| -> Result<Handle<Expression>, Refused> {
            let arg = args.get(i).ok_or_else(|| (format!("`{op:?}` with too few arguments"), why::OPERATIONS))?; // alloc: an error
            let v = cx.operand(frame, arg)?;
            cx.need(v)
        };
        Ok(Some(match op {
            CoreOp::Math(..) => unreachable!("math calls are Target::Math"),
            CoreOp::RoundHalfAway => {
                // trunc(x), one further from zero where what it cut off is at least a half: exact,
                // since x - trunc(x) loses no bits.
                let x = value(self, 0)?;
                let t = self.expr(Expression::Math { fun: MathFunction::Trunc, arg: x, arg1: None, arg2: None, arg3: None });
                let cut = self.expr(Expression::Binary { op: BinaryOperator::Subtract, left: x, right: t });
                let cut = self.expr(Expression::Math { fun: MathFunction::Abs, arg: cut, arg1: None, arg2: None, arg3: None });
                let half = self.expr(Expression::Literal(Literal::F32(0.5)));
                let up = self.expr(Expression::Binary { op: BinaryOperator::GreaterEqual, left: cut, right: half });
                let sign = self.expr(Expression::Math { fun: MathFunction::Sign, arg: x, arg1: None, arg2: None, arg3: None });
                let away = self.expr(Expression::Binary { op: BinaryOperator::Add, left: t, right: sign });
                self.expr(Expression::Select { condition: up, accept: away, reject: t })
            }
            CoreOp::Bin(bin) => {
                let (ty, rhs_ty) = (arg_ty(self, 0)?, arg_ty(self, 1)?);
                let (left, right) = (value(self, 0)?, value(self, 1)?);
                self.binary(bin, left, right, ty, rhs_ty, dest_ty, false)?
            }
            CoreOp::Bitcast => {
                let from = arg_ty(self, 0)?;
                let v = value(self, 0)?;
                self.bitcast(v, from, dest_ty)?
            }
            CoreOp::FloatToInt => {
                let to = self.scalar_of(dest_ty)?;
                let v = value(self, 0)?;
                self.expr(Expression::As { expr: v, kind: to.kind, convert: Some(4) })
            }
            CoreOp::Metadata => {
                let ptr_ty = arg_ty(self, 0)?;
                let Some(TyKind::Slice(_)) = pointee(ptr_ty).map(|p| self.dev.normalize(p).kind()) else {
                    // A thin pointer's metadata is `()`.
                    return Ok(None);
                };
                let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &args[0].kind else {
                    return Err(("the length of a constant slice".to_owned(), why::POINTERS)); // alloc: an error
                };
                let sym = self.sym_of_ref_value(frame, of)?;
                self.sym_len(&sym)?
            }
            CoreOp::Read | CoreOp::Write => {
                let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &args.first().ok_or_else(|| (format!("`{op:?}` without a pointer"), why::POINTERS))?.kind else { // alloc: an error
                    return Err((format!("`{op:?}` through a constant pointer"), why::POINTERS)); // alloc: an error
                };
                let sym = self.sym_of_ref_value(frame, of)?;
                let Some(pointer) = self.sym_ptr(&sym) else { return Ok(None) };
                if *op == CoreOp::Read {
                    self.expr(Expression::Load { pointer })
                } else {
                    let v = self.operand(frame, args.get(1).ok_or_else(|| ("a write without a value".to_owned(), why::OPERATIONS))?)?; // alloc: an error
                    if let Some(v) = v {
                        self.stmt(Statement::Store { pointer, value: v });
                    }
                    return Ok(None);
                }
            }
            CoreOp::Forget => return Ok(None),
        }))
    }

    /// A reference argument's place, as the handle it holds (a texture, a sampler).
    fn handle_arg(&mut self, frame: usize, arg: &m::Operand) -> Result<Handle<Expression>, Refused> {
        let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &arg.kind else {
            return Err(("a texture or sampler that is a constant".to_owned(), why::POINTERS)); // alloc: an error
        };
        let sym = self.sym_of_ref_value(frame, of)?;
        if !sym.steps.is_empty() {
            return Err(("a texture or sampler inside a value".to_owned(), why::TYPES)); // alloc: an error
        }
        self.sym_ptr(&sym).ok_or_else(|| ("a texture or sampler of no size".to_owned(), why::TYPES)) // alloc: an error
    }

    /// A device function's value; `None` for one that returns nothing (a store).
    fn device_call(&mut self, frame: usize, device: DeviceFn, args: &[m::Operand], dest_ty: Ty<'db>) -> Result<Option<Handle<Expression>>, Refused> {
        let value = |cx: &mut Self, i: usize| -> Result<Handle<Expression>, Refused> {
            let arg = args.get(i).ok_or_else(|| (format!("`{device:?}` with too few arguments"), why::OPERATIONS))?; // alloc: an error
            let v = cx.operand(frame, arg)?;
            cx.need(v)
        };
        let raw = |cx: &mut Self| -> Result<Handle<Expression>, Refused> {
            let arg = args.first().ok_or_else(|| (format!("`{device:?}` without its RawOut"), why::OPERATIONS))?; // alloc: an error
            let (m::OperandKind::Copy(of) | m::OperandKind::Move(of)) = &arg.kind else {
                return Err(("a RawOut that is a constant".to_owned(), why::POINTERS)); // alloc: an error
            };
            let sym = cx.sym_of_ref_value(frame, of)?;
            if !sym.steps.is_empty() {
                return Err(("a RawOut inside a value".to_owned(), why::TYPES)); // alloc: an error
            }
            cx.sym_ptr(&sym).ok_or_else(|| ("a RawOut of no size".to_owned(), why::TYPES)) // alloc: an error
        };
        Ok(Some(match device {
            DeviceFn::RawOutLen => {
                let pointer = raw(self)?;
                self.expr(Expression::ArrayLength(pointer))
            }
            DeviceFn::RawOutLoad => {
                let base = raw(self)?;
                let index = value(self, 1)?;
                let pointer = self.expr(Expression::Access { base, index });
                self.expr(Expression::Load { pointer })
            }
            DeviceFn::RawOutStore => {
                let base = raw(self)?;
                let index = value(self, 1)?;
                let pointer = self.expr(Expression::Access { base, index });
                // A value of no size stores nothing.
                if let Some(v) = self.operand(frame, args.get(2).ok_or_else(|| ("a RawOut write without a value".to_owned(), why::OPERATIONS))?)? {
                    // alloc: an error
                    self.stmt(Statement::Store { pointer, value: v });
                }
                return Ok(None);
            }
            DeviceFn::F32ToF16 => {
                let x = value(self, 0)?;
                let vec2 = self.value_ty(ValueTy::Vec2);
                let zero = self.expr(Expression::Literal(Literal::F32(0.0)));
                let pair = self.expr(Expression::Compose { ty: vec2, components: vec![x, zero] }); // alloc: two components
                self.expr(Expression::Math { fun: MathFunction::Pack2x16float, arg: pair, arg1: None, arg2: None, arg3: None })
            }
            DeviceFn::F16ToF32 => {
                let x = value(self, 0)?;
                let pair = self.expr(Expression::Math { fun: MathFunction::Unpack2x16float, arg: x, arg1: None, arg2: None, arg3: None });
                self.expr(Expression::AccessIndex { base: pair, index: 0 })
            }
            DeviceFn::TlasQuery => return self.tlas_query(frame, args, dest_ty).map(Some),
            DeviceFn::Derivative(axis) => {
                let expr = value(self, 0)?;
                self.expr(Expression::Derivative { axis, ctrl: naga::DerivativeControl::None, expr })
            }
            DeviceFn::Sample | DeviceFn::SampleLevel => {
                let image = self.handle_arg(frame, &args[0])?;
                let sampler = self.handle_arg(frame, args.get(1).ok_or_else(|| ("a sample without a sampler".to_owned(), why::OPERATIONS))?)?; // alloc: an error
                let coordinate = value(self, 2)?;
                let level = match device {
                    DeviceFn::SampleLevel => naga::SampleLevel::Exact(value(self, 3)?),
                    _ => naga::SampleLevel::Auto,
                };
                self.expr(Expression::ImageSample { image, sampler, gather: None, coordinate, array_index: None, offset: None, level, depth_ref: None, clamp_to_edge: false })
            }
            DeviceFn::Load => {
                let image = self.handle_arg(frame, &args[0])?;
                let coordinate = value(self, 1)?;
                let level = value(self, 2)?;
                self.expr(Expression::ImageLoad { image, coordinate, array_index: None, sample: None, level: Some(level) })
            }
            DeviceFn::Discard => unreachable!("discard ends the invocation"),
        }))
    }

    /// `Tlas::query(&self, ray, flags, mask)`: one naga ray query in a variable of its own,
    /// initialized from the ray, proceeded until it has no candidate left (every one is opaque,
    /// so none needs the kernel's say), and its committed intersection read into the gpu crate's
    /// `Committed`, each field from the `RayIntersection` member of its name.
    fn tlas_query(&mut self, frame: usize, args: &[m::Operand], dest_ty: Ty<'db>) -> Result<Handle<Expression>, Refused> {
        let [tlas, ray, flags, mask] = args else {
            return Err(("`Tlas::query` without its four arguments".to_owned(), why::OPERATIONS)); // alloc: an error
        };
        let acceleration_structure = self.handle_arg(frame, tlas)?;
        let ray_ty = self.operand_ty(frame, ray).ok_or_else(|| ("a static ray".to_owned(), why::STATICS))?; // alloc: an error
        let ray = self.operand(frame, ray)?;
        let ray = self.need(ray)?;
        let flags = self.operand(frame, flags)?;
        let flags = self.need(flags)?;
        let mask = self.operand(frame, mask)?;
        let mask = self.need(mask)?;

        // The ray's fields by name, into naga's RayDesc.
        let ray_shape = self.dev.lower_ty(ray_ty)?.shape;
        let Shape::Struct(ray_map) = ray_shape else {
            return Err((format!("the ray `{ty}` is not a struct", ty = self.dev.shown(ray_ty)), why::INTERFACE)); // alloc: an error
        };
        let ray_names = self.field_names(ray_ty);
        let ray_field = |cx: &mut Self, name: &str| -> Result<Handle<Expression>, Refused> {
            let i = ray_names.iter().position(|n| n == name).ok_or_else(|| (format!("a ray without its `{name}`"), why::INTERFACE))?; // alloc: an error
            let k = ray_map[i].ok_or_else(|| (format!("the ray's `{name}` of no size"), why::INTERFACE))?; // alloc: an error
            Ok(cx.expr(Expression::AccessIndex { base: ray, index: k }))
        };
        let (tmin, tmax, origin, dir) = (ray_field(self, "tmin")?, ray_field(self, "tmax")?, ray_field(self, "origin")?, ray_field(self, "dir")?);
        let desc_ty = self.dev.module.generate_ray_desc_type();
        // RayDesc's members: flags, cull_mask, tmin, tmax, origin, dir.
        let descriptor = self.expr(Expression::Compose { ty: desc_ty, components: vec![flags, mask, tmin, tmax, origin, dir] }); // alloc: six members
        let query_ty = self.dev.insert_ty(None, TypeInner::RayQuery { vertex_return: false });
        let var = self.func.local_variables.append(LocalVariable { name: Some("query".to_owned()), ty: query_ty, init: None }, Span::UNDEFINED); // alloc: a name
        let query = self.expr(Expression::LocalVariable(var));
        self.stmt(Statement::RayQuery { query, fun: naga::RayQueryFunction::Initialize { acceleration_structure, descriptor } });

        // loop { proceed; if !more { break } }
        self.blocks.push(naga::Block::new());
        let query = self.expr(Expression::LocalVariable(var));
        // The statement's result, which no Emit may cover.
        let more = self.func.expressions.append(Expression::RayQueryProceedResult, Span::UNDEFINED);
        self.stmt(Statement::RayQuery { query, fun: naga::RayQueryFunction::Proceed { result: more } });
        let done = self.expr(Expression::Unary { op: UnaryOperator::LogicalNot, expr: more });
        let mut stop = naga::Block::new();
        stop.push(Statement::Break, Span::UNDEFINED);
        self.stmt(Statement::If { condition: done, accept: stop, reject: naga::Block::new() });
        let body = self.blocks.pop().expect("the query's loop");
        self.stmt(Statement::Loop { body, continuing: naga::Block::new(), break_if: None });

        let intersection_ty = self.dev.module.generate_ray_intersection_type();
        let query = self.expr(Expression::LocalVariable(var));
        let hit = self.expr(Expression::RayQueryGetIntersection { query, committed: true });
        let TypeInner::Struct { members: hit_members, .. } = &self.dev.module.types[intersection_ty].inner else {
            unreachable!("naga's RayIntersection is a struct")
        };
        let hit_names: Vec<String> = hit_members.iter().map(|m| m.name.clone().unwrap_or_default()).collect(); // clone: names

        let lowered = self.dev.lower_ty(dest_ty)?;
        let (Some(naga_ty), Shape::Struct(map)) = (lowered.naga, lowered.shape) else {
            return Err((format!("a query read into `{ty}`, which is not a struct", ty = self.dev.shown(dest_ty)), why::INTERFACE)); // alloc: an error
        };
        let names = self.field_names(dest_ty);
        let mut members: Vec<(u32, Handle<Expression>)> = Vec::with_capacity(names.len()); // alloc: members
        for (i, name) in names.iter().enumerate() {
            let Some(k) = map[i] else { continue };
            // naga's name for the instance's own value.
            let naga_name = if name == "instance_custom_index" { "instance_custom_data" } else { name.as_str() };
            let Some(at) = hit_names.iter().position(|n| n == naga_name) else {
                return Err((format!("`{name}`, which naga's RayIntersection has no member for"), why::INTERFACE)); // alloc: an error
            };
            members.push((k, self.expr(Expression::AccessIndex { base: hit, index: at as u32 })));
        }
        members.sort_by_key(|(k, _)| *k);
        Ok(self.expr(Expression::Compose { ty: naga_ty, components: members.into_iter().map(|(_, e)| e).collect() }))
    }

    /// A node's terminator as the choice of its arms: an `if` chain (naga's `switch` would take
    /// the structurizer's `break`).
    fn select(&mut self, node: u32, arms: &[Vec<Shaped>]) {
        let NodeKind::Block { frame, block } = self.nodes[node as usize].kind.clone() else { return }; // clone: a small node
        let body = self.frames[frame].body;
        let Some(term) = &body.basic_blocks[block].terminator else { return };
        let conditions = match self.conditions(frame, &term.kind, arms.len()) {
            Ok(c) => c,
            Err(refused) => return self.refuse(frame, refused),
        };
        // `if c0 { arm0 } else if c1 { arm1 } ... else { last }`
        let last = arms.len() - 1;
        let mut reject = self.nested(&arms[last]);
        for i in (0..last).rev() {
            let accept = self.nested(&arms[i]);
            let mut outer = naga::Block::new();
            std::mem::swap(&mut outer, &mut reject);
            // The condition is computed where the chain starts, so every arm sees it.
            reject = naga::Block::new();
            let mut chain = naga::Block::new();
            chain.push(Statement::If { condition: conditions[i], accept, reject: outer }, Span::UNDEFINED);
            reject = chain;
        }
        let chain = std::mem::take(&mut reject);
        for (statement, span) in chain.span_into_iter() {
            self.block().push(statement, span);
        }
    }

    /// One condition per arm but the last, computed here.
    fn conditions(&mut self, frame: usize, kind: &m::TerminatorKind, arms: usize) -> Result<Vec<Handle<Expression>>, Refused> {
        Ok(match kind {
            m::TerminatorKind::SwitchInt { discr, targets } => {
                let ty = self.operand_ty(frame, discr).ok_or_else(|| ("a switch on a static".to_owned(), why::STATICS))?; // alloc: an error
                let scalar = self.scalar_of(ty)?;
                let d = self.operand(frame, discr)?;
                let d = self.need(d)?;
                let mut out = Vec::with_capacity(arms); // alloc: conditions
                for (value, _) in targets.iter() {
                    let c = match scalar.kind {
                        ScalarKind::Bool if value == 0 => self.expr(Expression::Unary { op: UnaryOperator::LogicalNot, expr: d }),
                        ScalarKind::Bool => d,
                        ScalarKind::Sint => {
                            let v = self.expr(Expression::Literal(Literal::I32(value as u32 as i32)));
                            self.expr(Expression::Binary { op: BinaryOperator::Equal, left: d, right: v })
                        }
                        _ => {
                            let v = self.expr(Expression::Literal(Literal::U32(value as u32)));
                            self.expr(Expression::Binary { op: BinaryOperator::Equal, left: d, right: v })
                        }
                    };
                    out.push(c);
                }
                out
            }
            m::TerminatorKind::Assert { cond, expected, .. } => {
                let c = self.operand(frame, cond)?;
                let c = self.need(c)?;
                let ok = if *expected { c } else { self.expr(Expression::Unary { op: UnaryOperator::LogicalNot, expr: c }) };
                vec![ok] // alloc: one condition
            }
            other => return Err((format!("a choice at the terminator `{other:?}`"), why::CONTROL)), // alloc: an error
        })
    }
}

/// The successors of a terminator that is not a call or return, as control goes (no unwind
/// edges: a panic ends the invocation).
fn normal_successors(kind: &m::TerminatorKind) -> Vec<m::BasicBlockId> {
    match kind {
        m::TerminatorKind::Goto { target }
        | m::TerminatorKind::FalseEdge { real_target: target, .. }
        | m::TerminatorKind::FalseUnwind { real_target: target, .. }
        | m::TerminatorKind::Drop { target, .. }
        | m::TerminatorKind::Assert { target, .. } => vec![*target], // alloc: an edge
        m::TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().to_vec(), // alloc: edges
        _ => Vec::new(),
    }
}

/// The blocks reached from `start` along `succ` without entering a doomed one, in reverse
/// postorder.
fn reverse_postorder(start: m::BasicBlockId, succ: &[Vec<m::BasicBlockId>], doomed: &[bool]) -> Vec<m::BasicBlockId> {
    let index = |b: m::BasicBlockId| b.into_raw().into_u32() as usize;
    let n = succ.len();
    let mut seen = vec![false; n]; // alloc: a frame's walk
    let mut post = Vec::with_capacity(n); // alloc: as above
    let mut stack: Vec<(m::BasicBlockId, usize)> = vec![(start, 0)]; // alloc: as above
    seen[index(start)] = true;
    while let Some(top) = stack.last_mut() {
        let (b, i) = *top;
        let next = if doomed[index(b)] { None } else { succ[index(b)].get(i).copied() };
        match next {
            Some(s) => {
                top.1 += 1;
                if !seen[index(s)] {
                    seen[index(s)] = true;
                    stack.push((s, 0));
                }
            }
            None => {
                post.push(b);
                stack.pop();
            }
        }
    }
    post.reverse();
    post
}

/// The target of a `switchInt` when its value is 0 (false).
fn false_target(targets: &m::SwitchTargets) -> m::BasicBlockId {
    targets.iter().find(|(value, _)| *value == 0).map(|(_, t)| t).unwrap_or_else(|| targets.otherwise())
}

/// How many times `local` is written: by an assignment to it or into it, or as a call's
/// destination.
/// The locals the front end reads a discriminant into to switch on it (`discr_temp_place` in
/// hir-ty-mir's lowering): typed `i128` whatever the enum, and assigned only by
/// `Rvalue::Discriminant`. Nothing else is written to them, so the device holds each as its
/// `i32` tag, losing nothing; a local that is also written another way keeps its type.
fn discriminant_temps(body: &m::MirBody<'_>) -> Vec<m::LocalId> {
    let mut found: Vec<(m::LocalId, usize)> = Vec::new(); // alloc: a frame's discriminant temporaries
    for (_, block) in body.basic_blocks.iter() {
        for statement in block.statements.iter() {
            if let m::StatementKind::Assign(place, m::Rvalue::Discriminant(_)) = &statement.kind
                && place.projection.as_slice().is_empty()
                && matches!(body.locals[place.local].ty.as_ref().kind(), TyKind::Int(IntTy::I128))
            {
                match found.iter_mut().find(|(local, _)| *local == place.local) {
                    Some((_, n)) => *n += 1,
                    None => found.push((place.local, 1)),
                }
            }
        }
    }
    found.into_iter().filter(|&(local, n)| assignments(body, local) == n).map(|(local, _)| local).collect()
}

fn assignments(body: &m::MirBody<'_>, local: m::LocalId) -> usize {
    let mut count = 0;
    for (_, block) in body.basic_blocks.iter() {
        for statement in block.statements.iter() {
            if let m::StatementKind::Assign(place, _) = &statement.kind
                && place.local == local
            {
                count += 1;
            }
        }
        if let Some(term) = &block.terminator
            && let m::TerminatorKind::Call { destination, .. } = &term.kind
            && destination.local == local
        {
            count += 1;
        }
    }
    count
}

/// What a reference or raw pointer type points at.
fn pointee(ty: Ty<'_>) -> Option<Ty<'_>> {
    match ty.kind() {
        TyKind::Ref(_, inner, _) | TyKind::RawPtr(inner, _) => Some(inner),
        _ => None,
    }
}

/// A path's last segment, for a variable's name.
fn short_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
