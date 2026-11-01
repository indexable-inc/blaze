//! The one spelling of a type by content ([`crate::identity`]) against the two it replaced,
//! kept here as the spec: the code generator's `v2/hashes.rs` `Spell` and this crate's own
//! (the [`Old`] spellings). Within one revision all three tell the same types, constants and
//! argument lists apart and fail on the same ones. Across an edit, the identity follows the
//! code generator's, which a coroutine's binding names are in, and this crate's old one missed
//! them. `is_plain_data` is checked against the code generator's copy the same way.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, coroutine_code, interface_hash, item_key, key, layout_hash};
use hegel::TestCase;
use hegel::generators as gs;
use hir_def::{AdtId, CallableDefId, ModuleDefId};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, TyDefId,
    consteval::usize_const,
    db::HirDatabase,
    mir::is_by_move_coroutine,
    next_solver::{
        Binder, Const, ConstKind, DbInterner, GenericArg, GenericArgs, ParamEnv, Pattern, TermId,
        Ty, TyKind, TypingMode, ValTree,
        infer::{DbInternerInferExt, InferCtxt},
    },
};
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};

use super::{TestDB, alias, declarations, function};
use crate::{code, identity, is_plain_data};

const MINICORE: &str = "sized, copy, clone, drop, fn, send, future, unsize, coerce_unsized";

/// Every kind of type the spelling has an arm for that a fixture can name: by type aliases,
/// and by the locals of the functions' MIR (closures, coroutines, parameters).
const CORPUS: &str = r#"
pub struct P(pub u32);
pub struct Q(pub u32);
pub struct W<T>(pub T);
pub enum E { A(u8), B }
pub struct D(pub u32);
impl Drop for D {
    fn drop(&mut self) {}
}
pub struct HasD { pub d: D }
pub trait Show { fn show(&self) -> u32; }
impl Show for P { fn show(&self) -> u32 { self.0 } }

pub type T0 = bool;
pub type T1 = char;
pub type T2 = u8;
pub type T3 = u32;
pub type T4 = i32;
pub type T5 = f64;
pub type T6 = P;
pub type T7 = Q;
pub type T8 = W<P>;
pub type T9 = W<Q>;
pub type T10 = W<W<P>>;
pub type T11 = E;
pub type T12 = D;
pub type T13 = HasD;
pub type T14 = &'static P;
pub type T15 = &'static mut P;
pub type T16 = *const P;
pub type T17 = *mut P;
pub type T18 = [u8; 2];
pub type T19 = [u8; 3];
pub type T20 = &'static [u8];
pub type T21 = &'static str;
pub type T22 = (u8, P);
pub type T23 = (P, u8);
pub type T24 = ();
pub type T25 = fn(u32) -> u32;
pub type T26 = unsafe fn(u32) -> u32;
pub type T27 = extern "C" fn(u32) -> u32;
pub type T28 = for<'a> fn(&'a u32) -> &'a u32;
pub type T29 = &'static dyn Show;
pub type T30 = &'static (dyn Show + Send);
pub type T31 = &'static (dyn Send + Show);
pub type T32 = W<u8>;
pub type Same8 = W<P>;
pub type Same14 = &'static P;

pub fn closures() -> u32 {
    let add = |x: u32| x + 1;
    let also = |x: u32| x + 1;
    add(1) + also(2)
}
pub fn generic<T: Copy>(x: T) -> (T, T) {
    let pair = (x, x);
    pair
}
pub fn futures() {
    let one = async { let one = 1u32; one };
    let two = async { let two = 2u32; two };
    let _ = (one, two);
}
"#;

const ASYNC: &str = r#"
pub fn make() {
    let fut = async { let one = 1u32; one };
    let _ = fut;
}
"#;

/// The spelling a copy used before there was one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Old {
    /// `hir-ty-codegen`'s `v2/hashes.rs` `Spell` and `adt_content`.
    Codegen,
    /// `hir-ty-share`'s `identity.rs` before it became the one.
    Share,
}

impl Old {
    fn ty_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen type 1",
            Old::Share => "blaze v2 share identity 1",
        }
    }

    fn const_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen const 1",
            Old::Share => "blaze v2 share identity 1",
        }
    }

    fn adt_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen adt 1",
            Old::Share => "blaze v2 share adt 1",
        }
    }
}

fn old_adt(db: &Db, old: Old, adt: AdtId) -> Result<ContentId, String> {
    let describe = |it: blaze_items::Unhashable| it.describe(db);
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(old.adt_domain());
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(describe)?.0)
        .id(&layout_hash(db, adt).map_err(describe)?.0);
    let mut seen: HashSet<AdtId> = HashSet::default();
    seen.insert(adt);
    let mut next = vec![adt];
    let mut dropping: Vec<(ItemKey, ContentId)> = Vec::new();
    while let Some(at) = next.pop() {
        if let Some(imp) = hir_ty::drop::destructor(db, at) {
            let interface = interface_hash(db, ItemId::Impl(imp)).map_err(describe)?;
            dropping.push((item_key(db, ItemId::Adt(at)), interface.0));
        }
        for variant in at.variants(db) {
            for field in db.field_types(variant).values() {
                for arg in field.ty().skip_binder().walk() {
                    if let Some(ty) = arg.ty()
                        && let TyKind::Adt(def, _) = ty.kind()
                        && seen.insert(def.def_id())
                    {
                        next.push(def.def_id());
                    }
                }
            }
        }
    }
    dropping.sort_unstable();
    h.update(&(dropping.len() as u64).to_le_bytes());
    for (key, interface) in &dropping {
        h.id(&key.0).id(interface);
    }
    Ok(h.finish())
}

/// Both old spellings, into one buffer: they differ only where `old` is matched on.
struct OldSpell<'db> {
    db: &'db Db,
    old: Old,
    out: Vec<u8>,
}

impl<'db> OldSpell<'db> {
    fn new(db: &'db Db, old: Old) -> OldSpell<'db> {
        OldSpell { db, old, out: Vec::new() }
    }

    fn ty_of(db: &'db Db, old: Old, ty: Ty<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, old);
        spell.ty(ty)?;
        Ok(ContentId::of(old.ty_domain(), &spell.out))
    }

    fn konst_of(db: &'db Db, old: Old, konst: Const<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, old);
        spell.konst(konst)?;
        Ok(ContentId::of(old.const_domain(), &spell.out))
    }

    /// The old share spelling of an argument list (the code generator had none of its own
    /// beyond `args_of` over its `Hashes`).
    fn args_of(db: &'db Db, args: GenericArgs<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, Old::Share);
        spell.args(args)?;
        Ok(ContentId::of(Old::Share.ty_domain(), &spell.out))
    }

    fn describe(&self) -> impl Fn(blaze_items::Unhashable) -> String + 'db {
        let db = self.db;
        move |it| it.describe(db)
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.extend_from_slice(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.extend_from_slice(&id.0);
    }

    fn str(&mut self, text: &str) {
        self.len(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }

    fn item(&mut self, item: ItemId) {
        let key = item_key(self.db, item);
        self.id(&key.0);
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => self.u8(0),
            TyKind::Char => self.u8(1),
            TyKind::Int(it) => {
                self.u8(2);
                self.str(it.name_str());
            }
            TyKind::Uint(it) => {
                self.u8(3);
                self.str(it.name_str());
            }
            TyKind::Float(it) => {
                self.u8(4);
                self.str(it.name_str());
            }
            TyKind::Adt(def, args) => {
                self.u8(5);
                let content = old_adt(db, self.old, def.def_id())?;
                self.id(&content);
                self.args(args)?;
            }
            TyKind::Foreign(it) => {
                self.u8(6);
                self.item(ItemId::Alias(it.0));
            }
            TyKind::Str => self.u8(7),
            TyKind::Array(element, len) => {
                self.u8(8);
                self.ty(element)?;
                self.konst(len)?;
            }
            TyKind::Pat(base, pattern) => {
                self.u8(9);
                self.ty(base)?;
                self.pattern(pattern)?;
            }
            TyKind::Slice(element) => {
                self.u8(10);
                self.ty(element)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                self.u8(11);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::Ref(_, pointee, mutability) => {
                self.u8(12);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                self.u8(13);
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.u8(0);
                        self.item(ItemId::Function(it));
                    }
                    CallableDefId::StructId(it) => {
                        self.u8(1);
                        self.item(ItemId::Adt(it.into()));
                    }
                    CallableDefId::EnumVariantId(it) => {
                        self.u8(2);
                        self.id(&key::variant_key(db, it).0);
                    }
                }
                self.args(args)?;
            }
            TyKind::FnPtr(tys, header) => {
                self.u8(14);
                self.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
                match self.old {
                    Old::Codegen => self.str(&format!("{:?} {:?}", header.abi(), header.safety())),
                    Old::Share => {
                        self.out.extend_from_slice(format!("{:?}", header.abi()).as_bytes());
                        self.u8(0xff);
                        self.out.extend_from_slice(format!("{:?}", header.safety()).as_bytes());
                        self.u8(0xff);
                    }
                }
                self.u8(header.c_variadic() as u8);
            }
            TyKind::UnsafeBinder(inner) => {
                self.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                self.len(binder.bound_vars().len());
                self.ty(binder.skip_binder())?;
            }
            TyKind::Dynamic(predicates, _) => {
                self.u8(16);
                let mut each = Vec::new();
                for predicate in predicates.iter() {
                    let mut inner = OldSpell::new(db, self.old);
                    inner.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            inner.u8(0);
                            inner.item(ItemId::Trait(it.def_id.0));
                            inner.args(it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            inner.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => inner.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => inner.item(ItemId::Const(konst)),
                            }
                            inner.args(it.args)?;
                            match it.term.kind() {
                                TermKind::Ty(ty) => {
                                    inner.u8(0);
                                    inner.ty(ty)?;
                                }
                                TermKind::Const(konst) => {
                                    inner.u8(1);
                                    inner.konst(konst)?;
                                }
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            inner.u8(2);
                            inner.item(ItemId::Trait(it.0));
                        }
                    }
                    each.push(ContentId::of(self.old.ty_domain(), &inner.out));
                }
                each.sort_unstable();
                self.len(each.len());
                for it in &each {
                    self.id(it);
                }
            }
            TyKind::Closure(id, args) => {
                self.u8(17);
                self.id(&key::closure_key(db, id.0).0);
                self.args(args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                self.u8(18);
                self.id(&key::coroutine_closure_key(db, id.0).0);
                self.args(args)?;
            }
            TyKind::Coroutine(id, args) => {
                self.u8(19);
                self.id(&key::coroutine_key(db, id.0).0);
                let code = match self.old {
                    Old::Codegen => {
                        let code = match is_by_move_coroutine(db, id.0, args.as_coroutine()) {
                            true => code::of_coroutine_by_move(db, id.0),
                            false => code::of_coroutine(db, id.0),
                        };
                        *code.as_ref().map_err(ToString::to_string)?
                    }
                    Old::Share => coroutine_code(db, id.0).map_err(self.describe())?.0,
                };
                self.id(&code);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.id(&key::coroutine_key(db, id.0).0);
                let code = match self.old {
                    Old::Codegen => *code::of_coroutine(db, id.0).as_ref().map_err(ToString::to_string)?,
                    Old::Share => coroutine_code(db, id.0).map_err(self.describe())?.0,
                };
                self.id(&code);
                self.args(args)?;
            }
            TyKind::Never => self.u8(21),
            TyKind::Tuple(tys) => {
                self.u8(22);
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
            }
            TyKind::Param(param) => {
                self.u8(23);
                self.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                self.u8(24);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
                self.u8(match bound.kind {
                    BoundTyKind::Anon => 0,
                    BoundTyKind::Param(_) => 1,
                });
            }
            TyKind::Alias(_, alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(format!("{what} left unnormalized in `{ty:?}`"));
            }
            TyKind::Placeholder(_) => return Err(format!("a placeholder type `{ty:?}`")),
            TyKind::Infer(_) => return Err(format!("an inference variable `{ty:?}`")),
            TyKind::Error(_) => return Err("a type with an error".to_owned()),
        }
        Ok(())
    }

    fn bound_index(&mut self, index: BoundVarIndexKind) {
        match index {
            BoundVarIndexKind::Bound(depth) => {
                self.u8(0);
                self.u32(depth.as_u32());
            }
            BoundVarIndexKind::Canonical => self.u8(1),
        }
    }

    fn args(&mut self, args: GenericArgs<'db>) -> Result<(), String> {
        self.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(_) => self.u8(0),
                GenericArgKind::Type(ty) => {
                    self.u8(1);
                    self.ty(ty)?;
                }
                GenericArgKind::Const(konst) => {
                    self.u8(2);
                    self.konst(konst)?;
                }
            }
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), String> {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                self.u8(0);
                self.konst(start)?;
                self.konst(end)?;
            }
            PatternKind::Or(patterns) => {
                self.u8(1);
                self.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(it)?;
                }
            }
            PatternKind::NotNull => self.u8(2),
        }
        Ok(())
    }

    fn konst(&mut self, konst: Const<'db>) -> Result<(), String> {
        match konst.kind() {
            ConstKind::Value(value) => {
                self.u8(0);
                self.ty(value.ty)?;
                self.valtree(value.value)?;
            }
            ConstKind::Param(param) => {
                self.u8(1);
                self.u32(param.index);
            }
            ConstKind::Bound(index, bound) => {
                self.u8(2);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
            }
            ConstKind::Alias(..) => return Err(format!("the constant `{konst:?}` is not evaluated")),
            ConstKind::Expr(_) => return Err(format!("the constant expression `{konst:?}`")),
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(format!("an inference variable `{konst:?}`"));
            }
            ConstKind::Placeholder(_) => return Err(format!("a placeholder constant `{konst:?}`")),
            ConstKind::Error(_) => return Err("a constant with an error".to_owned()),
        }
        Ok(())
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), String> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.extend_from_slice(&scalar.size().bytes().to_le_bytes());
                self.out.extend_from_slice(&scalar.to_bits_unchecked().to_le_bytes());
            }
            ValTreeKind::Branch(consts) => {
                self.u8(1);
                self.len(consts.len());
                for it in consts.iter() {
                    self.konst(it)?;
                }
            }
        }
        Ok(())
    }
}

/// The code generator's `FunctionCx::is_plain_data` before there was one, with its
/// `normalize`: the spec [`is_plain_data`] is held to.
fn old_is_plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
    depth: u32,
) -> bool {
    if depth > 8 {
        return false;
    }
    let ty = match ty.kind() {
        TyKind::Alias(..) => hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(infcx)
            .structurally_normalize_ty(
                &hir_ty::next_solver::infer::traits::ObligationCause::dummy(),
                param_env,
                ty,
            )
            .unwrap_or(ty),
        _ => ty,
    };
    match ty.kind() {
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
            old_is_plain_data(db, interner, infcx, param_env, element, depth + 1)
        }
        TyKind::Tuple(fields) => fields
            .iter()
            .all(|it| old_is_plain_data(db, interner, infcx, param_env, it, depth + 1)),
        TyKind::Adt(def, args) => {
            let variants: Vec<hir_def::VariantId> = match def.def_id() {
                AdtId::StructId(id) => vec![id.into()],
                AdtId::EnumId(id) => {
                    id.enum_variants(db).variants.iter().map(|(_, it)| it.0.into()).collect()
                }
                AdtId::UnionId(_) => return false,
            };
            variants.into_iter().all(|variant| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    old_is_plain_data(db, interner, infcx, param_env, ty, depth + 1)
                })
            })
        }
        _ => false,
    }
}

fn fixture(text: &str) -> TestDB {
    fixture::db(&format!("//- minicore: {MINICORE}\n{text}")).into_db()
}

/// The types of the corpus: each type alias's, then the type of each local of each function.
fn corpus<'db>(db: &'db TestDB) -> Vec<Ty<'db>> {
    let mut tys = Vec::new();
    for def in declarations(db) {
        match def {
            ModuleDefId::TypeAliasId(it) => tys.push(db.ty(TyDefId::TypeAliasId(it)).skip_binder()),
            ModuleDefId::FunctionId(it) => {
                let body = db
                    .mir_body(InferBodyId::DefWithBodyId(it.into()))
                    .unwrap_or_else(|e| panic!("the MIR of {it:?}: {e:?}"));
                tys.extend(body.locals.iter().map(|(_, local)| local.ty.as_ref()));
            }
            _ => {}
        }
    }
    tys
}

/// Whether `a` and `b` fail on the same entries and tell the same pairs apart.
#[track_caller]
fn agree<A: PartialEq, B: PartialEq>(what: &str, names: &[String], a: &[Option<A>], b: &[Option<B>]) {
    assert_eq!(a.len(), b.len());
    for i in 0..a.len() {
        assert_eq!(a[i].is_some(), b[i].is_some(), "{what}: `{}` is spelled by one only", names[i]);
        for j in 0..i {
            if let (Some(ai), Some(aj), Some(bi), Some(bj)) = (&a[i], &a[j], &b[i], &b[j]) {
                assert_eq!(ai == aj, bi == bj, "{what}: `{}` against `{}`", names[i], names[j]);
            }
        }
    }
}

/// Every spelling of every type of `tys`: the one, then the code generator's and share's old.
fn spellings<'db>(db: &'db Db, tys: &[Ty<'db>]) -> [Vec<Option<ContentId>>; 3] {
    [
        tys.iter().map(|&ty| identity::ty(db, ty).ok()).collect(),
        tys.iter().map(|&ty| OldSpell::ty_of(db, Old::Codegen, ty).ok()).collect(),
        tys.iter().map(|&ty| OldSpell::ty_of(db, Old::Share, ty).ok()).collect(),
    ]
}

#[track_caller]
fn all_agree<'db>(db: &'db Db, tys: &[Ty<'db>]) {
    let names: Vec<String> = tys.iter().map(|ty| format!("{ty:?}")).collect();
    let [one, codegen, share] = spellings(db, tys);
    agree("the code generator's old spelling", &names, &one, &codegen);
    agree("share's old spelling", &names, &one, &share);
}

#[test]
fn the_identity_tells_apart_what_both_old_spellings_did() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let tys = corpus(&db);
        all_agree(&db, &tys);
        let spelled = |name: &str| {
            identity::ty(&db, db.ty(TyDefId::TypeAliasId(alias(&db, name))).skip_binder())
                .unwrap_or_else(|why| panic!("`{name}`: {why}"))
        };
        assert_eq!(spelled("T8"), spelled("Same8"), "an alias is what it stands for");
        assert_eq!(spelled("T14"), spelled("Same14"));
        assert_eq!(spelled("T30"), spelled("T31"), "the order of a trait object's bounds");
        assert_ne!(spelled("T22"), spelled("T23"), "the order of a tuple's fields");
        assert_ne!(spelled("T25"), spelled("T26"), "a function pointer's safety");
        assert_ne!(spelled("T25"), spelled("T27"), "a function pointer's ABI");
        assert_ne!(spelled("T12"), spelled("T6"), "a drop impl");
    });
}

/// A length named by a constant or written as an expression is the value it evaluates to, as
/// rustc normalizes it: `[u8; N]` and `[u8; 1 + 1]` are `[u8; 2]`, one `TypeId`.
#[test]
fn a_named_or_computed_length_is_its_value() {
    let db = fixture(
        "pub const N: usize = 2;\n\
         pub type Two = [u8; 2];\n\
         pub type Named = [u8; N];\n\
         pub type Computed = [u8; 1 + 1];\n\
         pub type Three = [u8; N + 1];\n",
    );
    hir_ty::attach_db(&db, || {
        let spelled = |name: &str| {
            identity::ty(&db, db.ty(TyDefId::TypeAliasId(alias(&db, name))).skip_binder())
                .unwrap_or_else(|why| panic!("`{name}`: {why}"))
        };
        assert_eq!(spelled("Named"), spelled("Two"));
        assert_eq!(spelled("Computed"), spelled("Two"));
        assert_ne!(spelled("Three"), spelled("Two"));
    });
}

#[test]
fn constants_and_argument_lists_agree_with_the_old_spellings() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let krate = fixture::test_crate(&db);
        let konsts: Vec<Const<'_>> =
            [0, 1, 1, 2, u32::MAX as u128].map(|it| usize_const(&db, Some(it), krate)).to_vec();
        let names: Vec<String> = konsts.iter().map(|it| format!("{it:?}")).collect();
        let one: Vec<_> = konsts.iter().map(|&it| identity::konst(&db, it).ok()).collect();
        for old in [Old::Codegen, Old::Share] {
            let then: Vec<_> = konsts.iter().map(|&it| OldSpell::konst_of(&db, old, it).ok()).collect();
            agree(&format!("{old:?}'s constants"), &names, &one, &then);
        }

        let tys = corpus(&db);
        let lists: Vec<GenericArgs<'_>> = tys
            .iter()
            .flat_map(|&a| {
                [
                    GenericArgs::new_from_slice(&[GenericArg::from(a)]),
                    GenericArgs::new_from_slice(&[GenericArg::from(a), GenericArg::from(konsts[1])]),
                ]
            })
            .collect();
        let names: Vec<String> = lists.iter().map(|it| format!("{it:?}")).collect();
        let one: Vec<_> = lists.iter().map(|&it| identity::args(&db, it).ok()).collect();
        let then: Vec<_> = lists.iter().map(|&it| OldSpell::args_of(&db, it).ok()).collect();
        agree("share's argument lists", &names, &one, &then);
    });
}

/// The type of the one local of `func`'s MIR that is a coroutine.
fn coroutine_local<'db>(db: &'db TestDB, func: &str) -> Ty<'db> {
    let body = db
        .mir_body(InferBodyId::DefWithBodyId(function(db, func).into()))
        .unwrap_or_else(|e| panic!("the MIR of `{func}`: {e:?}"));
    let found: Vec<Ty<'db>> = body
        .locals
        .iter()
        .map(|(_, local)| local.ty.as_ref())
        .filter(|ty| matches!(ty.kind(), TyKind::Coroutine(..)))
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} coroutine locals in `{func}`", found.len()),
    }
}

/// Where the old spellings differed on purpose, and the one follows the finer: across a rename
/// of a binding a coroutine keeps, which its debug info names, the code generator's old
/// spelling and the one change, and share's old one did not (the missed input).
#[test]
fn a_coroutines_identity_holds_its_binding_names() {
    let fx = fixture::db(&format!("//- minicore: {MINICORE}\n{ASYNC}"));
    let file = fx.only_file();
    let mut db = fx.into_db();
    let file = file.file_id(&db);
    let spelled = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            let ty = coroutine_local(db, "make");
            (
                identity::ty(db, ty).expect("the one spelling"),
                OldSpell::ty_of(db, Old::Codegen, ty).expect("the code generator's"),
                OldSpell::ty_of(db, Old::Share, ty).expect("share's"),
            )
        })
    };
    let before = spelled(&db);
    assert_eq!(spelled(&db), before, "asked twice");

    db.set_file_text(file, &ASYNC.replace("one", "uno"));
    let after = spelled(&db);
    assert_ne!(after.0, before.0, "the one spelling holds the coroutine's binding names");
    assert_ne!(after.1, before.1, "as the code generator's did");
    assert_eq!(after.2, before.2, "share's old spelling missed them");
}

#[test]
fn is_plain_data_is_the_code_generators() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let krate = fixture::test_crate(&db);
        let interner = DbInterner::new_with(&db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
        let mut plain = 0;
        for ty in corpus(&db) {
            let one = is_plain_data(&db, interner, &infcx, env.param_env, ty);
            let old = old_is_plain_data(&db, interner, &infcx, env.param_env, ty, 0);
            assert_eq!(one, old, "`{ty:?}`");
            plain += usize::from(one);
        }
        assert!(plain > 0, "the corpus has plain data");
    });
}

/// A type written in the fixture's language: scalars, `P` and `Q` (one `u32` each), and what
/// references, pointers, arrays, tuples, `W<_>` and function pointers make of them.
/// At most `depth` levels of nesting, half the time stopping at a scalar.
fn ty_text(tc: &TestCase, depth: u32) -> String {
    if depth == 0 || !tc.draw(gs::booleans()) {
        let leaf = ["u8", "u32", "i64", "bool", "char", "P", "Q", "()", "&'static str"];
        return tc.draw(gs::sampled_from(leaf.to_vec())).to_owned();
    }
    let inner = || ty_text(tc, depth - 1);
    match tc.draw(gs::integers::<u8>().max_value(6)) {
        0 => format!("&'static {}", inner()),
        1 => format!("&'static mut {}", inner()),
        2 => format!("*const {}", inner()),
        3 => {
            let t = inner();
            format!("[{t}; {}]", tc.draw(gs::integers::<usize>().max_value(2)))
        }
        4 => format!("({}, {})", inner(), inner()),
        5 => format!("W<{}>", inner()),
        _ => format!("fn({}) -> {}", inner(), inner()),
    }
}

/// Random types, each written twice under two names, agree with both old spellings.
#[hegel::test(test_cases = 32)]
fn random_types_agree_with_both_old_spellings(tc: TestCase) {
    let len: usize = tc.draw(gs::integers().min_value(1).max_value(9));
    let texts: Vec<String> = (0..len).map(|_| ty_text(&tc, 3)).collect();
    let mut program = String::from("pub struct P(pub u32);\npub struct Q(pub u32);\npub struct W<T>(pub T);\n");
    for (at, text) in texts.iter().enumerate() {
        program.push_str(&format!("pub type A{at} = {text};\npub type B{at} = {text};\n"));
    }
    let db = fixture(&program);
    hir_ty::attach_db(&db, || {
        let tys: Vec<Ty<'_>> = (0..texts.len())
            .flat_map(|at| [format!("A{at}"), format!("B{at}")])
            .map(|name| db.ty(TyDefId::TypeAliasId(alias(&db, &name))).skip_binder())
            .collect();
        all_agree(&db, &tys);
        for pair in tys.chunks(2) {
            assert_eq!(identity::ty(&db, pair[0]).ok(), identity::ty(&db, pair[1]).ok());
        }
    });
}
