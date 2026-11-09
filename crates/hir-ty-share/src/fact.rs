//! The fact of a type: what code that moves, copies, projects or passes a value of it reads,
//! where that code does not tell types apart. Two types with one fact are handled by the same
//! machine code wherever only their facts are read.
//!
//! - An integer or a float is whether it is signed, unsigned or a float, and its layout:
//!   `u64` and `usize` have one fact on a 64-bit target, `u32` and `i32` do not (the layout's
//!   scalar has its signedness). `bool`, `char`, `str` and `!` are their identity.
//! - An ADT is its **shape** (`docs/v2/plan/design/polymorph.md`): whether it is a struct, an
//!   enum or a union, its `repr` (flags, integer, `align`, `packed`, scalable; not the field
//!   shuffle seed, whose effect is in the layout's offsets), its layout, its number of variants,
//!   each variant's discriminant and number of fields, and its fields' facts, with the
//!   arguments put in. So two ADTs that code handles alike (`struct A(u32)` and
//!   `struct B(u32)`) have one fact, and `Vec<&A>`, `Vec<&B>`, `Vec<A>` and `Vec<B>` too. What
//!   tells them apart is read where it is used, never through the fact: a call of one of their
//!   methods by the callee's key, a drop by the drop glue's identity ([`crate::drop`]), a
//!   vtable, `TypeId` or `type_name` by full identity ([`identity`]).
//! - An ADT that the code generator or the layout may treat by what it is rather than by its
//!   shape keeps its definition's identity ([`identity::adt`]: its key, interface, layout hash
//!   and drop impls) in place of the shape's kind and `repr`: a lang item (`Box`,
//!   `UnsafeCell`, `ManuallyDrop`, `MaybeUninit`, `PhantomData`, `Option`, ...), an ADT marked
//!   `#[rustc_pass_indirectly_in_non_rustic_abis]` or `#[rustc_layout_scalar_valid_range_*]`,
//!   and a SIMD or scalable vector. Its arguments are still read by their facts, so
//!   `Option<A>` and `Option<B>` have one fact.
//! - A tuple, an array and a closure are their layouts and their parts' facts.
//! - A pointer is its mutability and a summary of what it points to: its kind, size and
//!   alignment, whether it is `Freeze`, `Unpin` and plain data, which the code generator reads
//!   to say what memory a reference argument is the only way to. Never the pointee's fact: a
//!   type reaches itself only through a pointer, so a fact never cycles.
//! - A function item is a value of no size: what calls it, casts it to a pointer or makes a
//!   vtable of it keys it there (compile identity's walk, `hir_ty_identity::instance_identity`).
//! - Everything else (a function pointer, a trait object, a coroutine) is its identity: the
//!   code that handles it tells it apart.
//!
//! Each fact ends with whether the type needs drop and whether it is `Freeze`.

use crate::{FACT_DOMAIN, Unshared, identity, layout};
use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use hir_def::{AdtId, VariantId, attrs::AttrFlags};
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue, is_freeze},
    layout::Layout,
    next_solver::{
        DbInterner, ParamEnv, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::{StoredParamEnvAndCrate, implements_trait_unique, structurally_normalize_ty},
};
use rustc_abi::{IntegerType, ReprFlags, ReprOptions, ScalableElt};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

/// What a fact or a pointee summary is of: its first byte.
mod tag {
    pub(super) const LEAF: u8 = 1;
    pub(super) const ADT: u8 = 2;
    pub(super) const TUPLE: u8 = 3;
    pub(super) const ARRAY: u8 = 4;
    pub(super) const SLICE: u8 = 5;
    pub(super) const REF: u8 = 6;
    pub(super) const RAW: u8 = 7;
    pub(super) const CLOSURE: u8 = 8;
    pub(super) const IDENTITY: u8 = 9;
    pub(super) const FN_ITEM: u8 = 10;
    pub(super) const NUMBER: u8 = 11;
    pub(super) const ADT_SHAPE: u8 = 12;
    pub(super) const POINTEE_SIZED: u8 = 16;
    pub(super) const POINTEE_STR: u8 = 17;
    pub(super) const POINTEE_SLICE: u8 = 18;
    pub(super) const POINTEE_OPAQUE: u8 = 19;
}

/// How deep [`is_plain_data`] looks into a type.
const PLAIN_DATA_DEPTH: u32 = 8;

/// Whether `ty` is made of scalars, pointers and references only, through arrays, tuples and
/// the fields of structs and enums: what the code generator reads to say a reference is the
/// only way to its memory (its `reference_pointee`), and what a pointee's summary records.
/// Only plain data is looked at: whether a type with a coroutine, a closure or a trait object
/// in it is `Unpin` takes more than its fields to know, and a wrong yes is a miscompile.
pub fn is_plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
) -> bool {
    plain_data(db, interner, infcx, param_env, ty, 0)
}

fn plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
    depth: u32,
) -> bool {
    if depth > PLAIN_DATA_DEPTH {
        return false;
    }
    match structurally_normalize_ty(infcx, ty, param_env).kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Never
        | TyKind::RawPtr(..)
        | TyKind::Ref(..)
        | TyKind::FnPtr(..)
        | TyKind::FnDef(..) => true,
        TyKind::Array(element, _) | TyKind::Pat(element, _) => {
            plain_data(db, interner, infcx, param_env, element, depth + 1)
        }
        TyKind::Tuple(fields) => {
            fields.iter().all(|it| plain_data(db, interner, infcx, param_env, it, depth + 1))
        }
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if let hir_def::AdtId::UnionId(_) = adt {
                return false;
            }
            adt.variants(db).all(|variant| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    plain_data(db, interner, infcx, param_env, ty, depth + 1)
                })
            })
        }
        _ => false,
    }
}

/// What the queries of one environment share: the database, an inference context in it and
/// the environment, borrowed and stored.
pub struct Cx<'db> {
    pub db: &'db Db,
    pub interner: DbInterner<'db>,
    pub infcx: InferCtxt<'db>,
    pub env: ParamEnvAndCrate<'db>,
    pub stored: StoredParamEnvAndCrate,
}

impl<'db> Cx<'db> {
    pub fn new(db: &'db Db, stored: StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Cx { db, interner, infcx, env, stored }
    }

    /// `ty` with a top-level alias normalized, as the code generator's `normalize` does.
    pub fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        structurally_normalize_ty(&self.infcx, ty, self.env.param_env)
    }

    pub fn layout(&self, ty: Ty<'db>) -> Result<&'static Layout, Unshared> {
        self.db
            .layout_of_ty(ty.store(), self.stored.clone()) // clone: the query's key, a reference count
            .map(|layout| &layout.get().0)
            .map_err(|error| Unshared::NoLayout(format!("`{ty:?}`: {error:?}"))) // alloc: error path
    }

    pub fn needs_drop(&self, ty: Ty<'db>) -> bool {
        has_drop_glue(&self.infcx, ty, self.env.param_env) != DropGlue::None
    }

    /// `ty` with every alias in it normalized ([`crate::deeply_normalize`]).
    pub fn deeply_normalize(&self, ty: Ty<'db>) -> Result<Ty<'db>, Unshared> {
        crate::deeply_normalize(&self.infcx, self.env.param_env, ty)
    }

    /// The fact of `ty`: one query per (type, environment), so a hit is one lookup. A field's
    /// fact is a query inside its ADT's or tuple's, so a miss nests one query per level of
    /// `ty`, as deep as the program built it: each level runs with stack to spare (rule 20).
    pub fn fact(&self, ty: Ty<'db>) -> Result<ContentId, Unshared> {
        stdx::ensure_sufficient_stack(|| {
            let ty = self.deeply_normalize(ty)?;
            match fact(self.db, ty.store(), self.stored.clone()) {
                // clone: the query's key, a reference count
                Ok(id) => Ok(*id),
                Err(why) => Err(why.clone()), // clone: error path
            }
        })
    }

    fn is_unpin(&self, ty: Ty<'db>) -> bool {
        self.interner
            .lang_items()
            .Unpin
            .is_some_and(|unpin| implements_trait_unique(ty, self.db, self.env, unpin))
    }

    /// What a pointer to `pointee` says of its memory.
    fn pointee(&self, pointee: Ty<'db>, out: &mut Hasher) -> Result<(), Unshared> {
        let pointee = self.deeply_normalize(pointee)?;
        let (size, align) = match pointee.kind() {
            TyKind::Str => {
                out.update(&[tag::POINTEE_STR]);
                (0, 1)
            }
            TyKind::Slice(element) => {
                out.update(&[tag::POINTEE_SLICE]);
                let layout = self.layout(self.normalize(element))?;
                out.update(&layout.size.bytes().to_le_bytes());
                (0, layout.align.abi.bytes())
            }
            TyKind::Dynamic(..) | TyKind::Foreign(_) => {
                out.update(&[tag::POINTEE_OPAQUE]);
                out.update(&identity::ty(self.db, pointee)?.0);
                (0, 1)
            }
            _ => {
                let layout = self.layout(pointee)?;
                if layout.is_unsized() {
                    // A struct with an unsized tail: its metadata is the tail's.
                    out.update(&[tag::POINTEE_OPAQUE]);
                    out.update(&identity::ty(self.db, pointee)?.0);
                } else {
                    out.update(&[tag::POINTEE_SIZED]);
                    out.update(&[u8::from(layout.uninhabited)]);
                }
                (layout.size.bytes(), layout.align.abi.bytes())
            }
        };
        out.update(&size.to_le_bytes());
        out.update(&align.to_le_bytes());
        let element = match pointee.kind() {
            TyKind::Str => None,
            TyKind::Slice(element) => Some(element),
            _ => Some(pointee),
        };
        let plain = element.is_none_or(|it| {
            is_plain_data(self.db, self.interner, &self.infcx, self.env.param_env, it)
        });
        let freeze = is_freeze(&self.infcx, pointee, self.env.param_env);
        let unpin = self.is_unpin(pointee);
        out.update(&[u8::from(plain) | u8::from(freeze) << 1 | u8::from(unpin) << 2]);
        Ok(())
    }

    fn write(&self, ty: Ty<'db>, out: &mut Hasher) -> Result<(), Unshared> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Never => {
                out.update(&[tag::LEAF]);
                out.update(&identity::ty(db, ty)?.0);
            }
            // What the code generator reads of a number is its class and its layout's scalar
            // (size, alignment, signedness, valid range): `usize` is `u64` on a 64-bit target.
            TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) => {
                let class = match ty.kind() {
                    TyKind::Int(_) => 0u8,
                    TyKind::Uint(_) => 1,
                    _ => 2,
                };
                out.update(&[tag::NUMBER, class]);
                layout::write(self.layout(ty)?, out);
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                match keeps_identity(db, adt) {
                    true => {
                        out.update(&[tag::ADT]);
                        out.id(identity::adt(db, adt).as_ref().map_err(Clone::clone)?);
                    }
                    false => {
                        out.update(&[tag::ADT_SHAPE]);
                        write_shape(db, adt, out);
                    }
                }
                layout::write(self.layout(ty)?, out);
                // Each variant's discriminant and fields, then how many variants there are
                // (`variant_count`, and an enum of one inhabited variant whose layout is a
                // struct's).
                let mut variants = 0u64;
                for variant in adt.variants(db) {
                    variants += 1;
                    if let VariantId::EnumVariantId(variant) = variant {
                        let discriminant =
                            db.const_eval_discriminant(variant).map_err(|error| {
                                Unshared::ConstEval(format!(
                                    "a discriminant of `{ty:?}`: {error:?}"
                                )) // alloc: error path
                            })?;
                        out.update(&discriminant.to_le_bytes());
                    }
                    let fields = db.field_types(variant);
                    out.update(&(fields.len() as u64).to_le_bytes());
                    for (_, field) in fields.iter() {
                        let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        out.update(&self.fact(field)?.0);
                    }
                }
                out.update(&variants.to_le_bytes());
            }
            TyKind::Tuple(fields) => {
                out.update(&[tag::TUPLE]);
                layout::write(self.layout(ty)?, out);
                for field in fields.iter() {
                    out.update(&self.fact(field)?.0);
                }
            }
            TyKind::Array(element, len) => {
                out.update(&[tag::ARRAY]);
                layout::write(self.layout(ty)?, out);
                out.update(&self.fact(element)?.0);
                out.update(&identity::konst(db, len)?.0);
            }
            TyKind::Slice(element) => {
                out.update(&[tag::SLICE]);
                out.update(&self.fact(element)?.0);
            }
            TyKind::Ref(_, pointee, mutability) => {
                out.update(&[tag::REF]);
                out.update(&[u8::from(mutability.is_mut())]);
                self.pointee(pointee, out)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                out.update(&[tag::RAW]);
                out.update(&[u8::from(mutability.is_mut())]);
                self.pointee(pointee, out)?;
            }
            TyKind::Closure(_, args) => {
                out.update(&[tag::CLOSURE]);
                layout::write(self.layout(ty)?, out);
                out.update(&self.fact(args.as_closure().tupled_upvars_ty())?.0);
            }
            // A function item is a value of no size; a call of it, a cast of it to a pointer
            // and a vtable of it key it where they are.
            TyKind::FnDef(..) => {
                out.update(&[tag::FN_ITEM]);
            }
            // A parameter, an alias that did not normalize or an error has no identity, which
            // says so.
            _ => {
                out.update(&[tag::IDENTITY]);
                out.update(&identity::ty(db, ty)?.0);
            }
        }
        let freeze = is_freeze(&self.infcx, ty, self.env.param_env);
        out.update(&[u8::from(self.needs_drop(ty)) | u8::from(freeze) << 1]);
        Ok(())
    }
}

/// The attributes that make the code generator or the layout treat an ADT by what it is: a
/// lang item (`Box`'s deref and `noalias`, `UnsafeCell`'s `Freeze`, `ManuallyDrop`'s drop,
/// `MaybeUninit`'s validity), C's indirect passing, a scalar's valid range.
const IDENTIFYING: AttrFlags = AttrFlags::LANG_ITEM
    .union(AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS)
    .union(AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE);

/// Whether an ADT's fact holds its definition's identity rather than its shape: one the code
/// generator may read by what it is (`IDENTIFYING`), or a SIMD or scalable vector, whose
/// element the code generator reads off the definition.
fn keeps_identity(db: &Db, adt: AdtId) -> bool {
    AttrFlags::query(db, adt.into()).intersects(IDENTIFYING)
        || AttrFlags::repr(db, adt).is_some_and(|repr| {
            repr.flags.intersects(ReprFlags::IS_SIMD | ReprFlags::IS_SCALABLE)
                || repr.scalable.is_some()
        })
}

/// An ADT's shape before its layout and fields: its kind and its `repr`, which the code
/// generator reads for a C call (`repr(C)`, `transparent`, `packed`) and the layout already
/// holds the rest of. The field shuffle seed is left out: it names the definition, and what
/// it did is in the layout's field offsets.
fn write_shape(db: &Db, adt: AdtId, out: &mut Hasher) {
    let kind = match adt {
        AdtId::StructId(_) => 0u8,
        AdtId::EnumId(_) => 1,
        AdtId::UnionId(_) => 2,
    };
    out.update(&[kind]);
    let Some(repr) = AttrFlags::repr(db, adt) else {
        out.update(&[0]);
        return;
    };
    let ReprOptions { int, align, pack, flags, scalable, field_shuffle_seed: _ } = repr;
    out.update(&[1, flags.bits()]);
    match int {
        None => out.update(&[0]),
        Some(IntegerType::Pointer(signed)) => out.update(&[1, u8::from(signed)]),
        Some(IntegerType::Fixed(integer, signed)) => {
            out.update(&[2, integer.size().bytes() as u8, u8::from(signed)])
        }
    };
    for it in [align, pack] {
        match it {
            None => out.update(&[0]),
            Some(align) => out.update(&[1]).update(&align.bytes().to_le_bytes()),
        };
    }
    match scalable {
        None => out.update(&[0]),
        Some(ScalableElt::ElementCount(count)) => out.update(&[1]).update(&count.to_le_bytes()),
        Some(ScalableElt::Container) => out.update(&[2]),
    };
}

#[blaze_query::tracked(returns(ref), cycle_result = fact_cycle)]
fn fact(db: &Db, ty: StoredTy, env: StoredParamEnvAndCrate) -> Result<ContentId, Unshared> {
    let cx = Cx::new(db, env);
    let mut out = Hasher::new(FACT_DOMAIN);
    cx.write(ty.as_ref(), &mut out)?;
    Ok(out.finish())
}

fn fact_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<ContentId, Unshared> {
    Err(Unshared::Recursive)
}
