//! Unit tests for first-class field-quantified predicates (`Pred::ForFields`).

use core::mem::size_of;

use blaze_own::Vec as OwnVec;

use crate::assembly::{Clause, Goal};
use crate::canon::{Canon, Decode};
use crate::coherence::{Overlap, overlap};
use crate::error::Stop;
use crate::eval::{Answer, Solver};
use crate::facts::{
    AdtDef, AdtFlags, AssocDef, AssocKind, ConstHolds, Discr, Facts, Fetch, FnSigDef, ImplAssoc, ImplFlags,
    ImplHeader, ImplPolarity, ImplSet, Missing, OpaqueDef, Portable, Question, Read, Seal, TraitDef, TraitFlags,
};
use crate::prefilter::Columns;
use crate::term::{
    AliasKind, Certainty, Def, FloatTy, IntTy, Lang, List, Mutbl, Polarity, Pred, Rigid, Shape, Ty, TyKind,
    TypingMode, UintTy, ValTy, VarKind,
};
use crate::testing::tys::TestTys;
use crate::tys::Tys;

// Trait defs.
const TR_CODEC: Def = Def(0x1001);
const TR_TAG: Def = Def(0x1002);
const TR_COINDUCTIVE: Def = Def(0x1003);
const TR_ASSOC: Def = Def(0x1004);
const ASSOC_OUT: Def = Def(0x1005);

// ADT defs.
const ADT_POS: Def = Def(0x2001);
const ADT_UNIT: Def = Def(0x2002);
const ADT_MIXED: Def = Def(0x2003);
const ADT_BAD: Def = Def(0x2004);
const ADT_SHIP: Def = Def(0x2005);
const ADT_BAD_SHIP: Def = Def(0x2006);
const ADT_PAIR: Def = Def(0x2007);
const ADT_ENUM: Def = Def(0x2008);
const ADT_UNION: Def = Def(0x2009);
const ADT_SELF_REC: Def = Def(0x200a);
const ADT_WRAPPER: Def = Def(0x200b);
const ADT_MISSING: Def = Def(0x200c);

// Impl defs.
const IMPL_CODEC_I32: Def = Def(0x3001);
const IMPL_CODEC_BOOL: Def = Def(0x3002);
const IMPL_CODEC_BLANKET: Def = Def(0x3003);
const IMPL_CODEC_POS_EXPLICIT: Def = Def(0x3004);
const IMPL_TAG_I32_U8: Def = Def(0x3005);
const IMPL_TAG_BOOL_U8: Def = Def(0x3006);
const IMPL_COIND_I32: Def = Def(0x3007);
const IMPL_COIND_BLANKET: Def = Def(0x3008);
const IMPL_ASSOC_WRAPPER: Def = Def(0x3009);

// Portable tables for each ADT / impl.
static PORT_I32_I32: Portable<'static> = Portable {
    nodes: &[TyKind::Int(IntTy::I32)],
    lists: &[],
};
static FIELDS_I32_I32: &[Ty] = &[Ty(0), Ty(0)];
static VAR_2: &[(u32, u32)] = &[(0, 2)];

static FIELDS_EMPTY: &[Ty] = &[];
static VAR_0: &[(u32, u32)] = &[(0, 0)];

static PORT_I32_BOOL: Portable<'static> = Portable {
    nodes: &[TyKind::Int(IntTy::I32), TyKind::Bool],
    lists: &[],
};
static FIELDS_I32_BOOL: &[Ty] = &[Ty(0), Ty(1)];

static PORT_I32_F32: Portable<'static> = Portable {
    nodes: &[TyKind::Int(IntTy::I32), TyKind::Float(FloatTy::F32)],
    lists: &[],
};
static FIELDS_I32_F32: &[Ty] = &[Ty(0), Ty(1)];

static PORT_SHIP: Portable<'static> = Portable {
    nodes: &[TyKind::Adt(ADT_POS, List::EMPTY), TyKind::Bool],
    lists: &[],
};
static FIELDS_SHIP: &[Ty] = &[Ty(0), Ty(1)];

static PORT_BAD_SHIP: Portable<'static> = Portable {
    nodes: &[TyKind::Adt(ADT_POS, List::EMPTY), TyKind::Adt(ADT_BAD, List::EMPTY)],
    lists: &[],
};
static FIELDS_BAD_SHIP: &[Ty] = &[Ty(0), Ty(1)];

static PORT_PAIR: Portable<'static> = Portable {
    nodes: &[TyKind::Param(0)],
    lists: &[],
};
static FIELDS_PAIR: &[Ty] = &[Ty(0), Ty(0)];

static PORT_SELF_REC: Portable<'static> = Portable {
    nodes: &[TyKind::Adt(ADT_SELF_REC, List::EMPTY)],
    lists: &[],
};
static FIELDS_SELF_REC: &[Ty] = &[Ty(0)];
static VAR_1: &[(u32, u32)] = &[(0, 1)];

// Impl headers' portable tables.
static PORT_IMPL_I32: Portable<'static> = Portable {
    nodes: &[TyKind::Int(IntTy::I32)],
    lists: &[Ty(0)],
};
static PORT_IMPL_BOOL: Portable<'static> = Portable {
    nodes: &[TyKind::Bool],
    lists: &[Ty(0)],
};
static PORT_IMPL_POS: Portable<'static> = Portable {
    nodes: &[TyKind::Adt(ADT_POS, List::EMPTY)],
    lists: &[Ty(0)],
};
static PORT_IMPL_BLANKET: Portable<'static> = Portable {
    nodes: &[TyKind::Param(0)],
    lists: &[Ty(0)],
};
static WC_CODEC_BLANKET: &[Pred] = &[Pred::ForFields {
    container: Ty(0),
    trait_: TR_CODEC,
    args_after_self: List::EMPTY,
}];
static WC_COIND_BLANKET: &[Pred] = &[Pred::ForFields {
    container: Ty(0),
    trait_: TR_COINDUCTIVE,
    args_after_self: List::EMPTY,
}];
static PORT_IMPL_TAG_I32_U8: Portable<'static> = Portable {
    nodes: &[TyKind::Int(IntTy::I32), TyKind::Uint(UintTy::U8)],
    lists: &[Ty(0), Ty(1)],
};
static PORT_IMPL_TAG_BOOL_U8: Portable<'static> = Portable {
    nodes: &[TyKind::Bool, TyKind::Uint(UintTy::U8)],
    lists: &[Ty(0), Ty(1)],
};
static PORT_IMPL_ASSOC_WRAPPER: Portable<'static> = Portable {
    nodes: &[TyKind::Adt(ADT_WRAPPER, List::EMPTY), TyKind::Adt(ADT_POS, List::EMPTY)],
    lists: &[Ty(0)],
};
static ASSOC_WRAPPER_ITEMS: &[ImplAssoc] = &[ImplAssoc {
    trait_item: ASSOC_OUT,
    value: Ty(1),
    own_generics: 0,
    is_default: false,
}];
static TRAIT_ASSOC_ITEMS: &[crate::facts::AssocTy] = &[crate::facts::AssocTy {
    def: ASSOC_OUT,
    own_generics: 0,
    bounds_start: 0,
    bounds_len: 0,
    default: None,
}];

const EMPTY_COLS: Columns<'static> = Columns { bytes: &[], stride: 0, len: 0 };

struct World {
    with_explicit_pos_impl: bool,
}

impl World {
    fn new() -> Self {
        Self { with_explicit_pos_impl: false }
    }
}

impl Facts for World {
    fn trait_def(&self, _: Seal, t: Def) -> Fetch<TraitDef<'_>> {
        let (flags, generics, assoc) = match t {
            TR_CODEC => (TraitFlags::default(), 1, &[][..]),
            TR_TAG => (TraitFlags::default(), 2, &[][..]),
            TR_COINDUCTIVE => (TraitFlags(TraitFlags::COINDUCTIVE), 1, &[][..]),
            TR_ASSOC => (TraitFlags::default(), 1, TRAIT_ASSOC_ITEMS),
            _ => return Err(Missing),
        };
        Ok(TraitDef {
            flags,
            generics,
            lang: None,
            supertraits: &[],
            where_clauses: &[],
            assoc,
            item_bounds: &[],
            portable: Portable::EMPTY,
            digest: t.0 as u64,
        })
    }

    fn impls(&self, _: Seal, t: Def, shape: Shape) -> Fetch<ImplSet<'_>> {
        let impls: &'static [Def] = match (t, shape) {
            (TR_CODEC, Shape::Int(IntTy::I32)) => &[IMPL_CODEC_I32, IMPL_CODEC_BLANKET],
            (TR_CODEC, Shape::Bool) => &[IMPL_CODEC_BOOL, IMPL_CODEC_BLANKET],
            (TR_CODEC, Shape::Adt(ADT_POS)) if self.with_explicit_pos_impl => {
                &[IMPL_CODEC_POS_EXPLICIT, IMPL_CODEC_BLANKET]
            }
            (TR_CODEC, _) => &[IMPL_CODEC_BLANKET],
            (TR_TAG, Shape::Int(IntTy::I32)) => &[IMPL_TAG_I32_U8],
            (TR_TAG, Shape::Bool) => &[IMPL_TAG_BOOL_U8],
            (TR_TAG, _) => &[],
            (TR_COINDUCTIVE, Shape::Int(IntTy::I32)) => &[IMPL_COIND_I32, IMPL_COIND_BLANKET],
            (TR_COINDUCTIVE, _) => &[IMPL_COIND_BLANKET],
            (TR_ASSOC, Shape::Adt(ADT_WRAPPER)) => &[IMPL_ASSOC_WRAPPER],
            (TR_ASSOC, _) => &[],
            _ => &[],
        };
        Ok(ImplSet { impls, columns: EMPTY_COLS, digest: impls.len() as u64 })
    }

    fn impl_header(&self, _: Seal, i: Def) -> Fetch<ImplHeader<'_>> {
        let h = match i {
            IMPL_CODEC_I32 => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_CODEC,
                args: List { start: 0, len: 1 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_I32,
                digest: i.0 as u64,
            },
            IMPL_CODEC_BOOL => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_CODEC,
                args: List { start: 0, len: 1 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_BOOL,
                digest: i.0 as u64,
            },
            IMPL_CODEC_BLANKET => ImplHeader {
                generics: 1,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_CODEC,
                args: List { start: 0, len: 1 },
                where_clauses: WC_CODEC_BLANKET,
                assoc: &[],
                portable: PORT_IMPL_BLANKET,
                digest: i.0 as u64,
            },
            IMPL_CODEC_POS_EXPLICIT => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_CODEC,
                args: List { start: 0, len: 1 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_POS,
                digest: i.0 as u64,
            },
            IMPL_TAG_I32_U8 => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_TAG,
                args: List { start: 0, len: 2 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_TAG_I32_U8,
                digest: i.0 as u64,
            },
            IMPL_TAG_BOOL_U8 => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_TAG,
                args: List { start: 0, len: 2 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_TAG_BOOL_U8,
                digest: i.0 as u64,
            },
            IMPL_COIND_I32 => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_COINDUCTIVE,
                args: List { start: 0, len: 1 },
                where_clauses: &[],
                assoc: &[],
                portable: PORT_IMPL_I32,
                digest: i.0 as u64,
            },
            IMPL_COIND_BLANKET => ImplHeader {
                generics: 1,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_COINDUCTIVE,
                args: List { start: 0, len: 1 },
                where_clauses: WC_COIND_BLANKET,
                assoc: &[],
                portable: PORT_IMPL_BLANKET,
                digest: i.0 as u64,
            },
            IMPL_ASSOC_WRAPPER => ImplHeader {
                generics: 0,
                polarity: ImplPolarity::Positive,
                flags: ImplFlags::default(),
                trait_: TR_ASSOC,
                args: List { start: 0, len: 1 },
                where_clauses: &[],
                assoc: ASSOC_WRAPPER_ITEMS,
                portable: PORT_IMPL_ASSOC_WRAPPER,
                digest: i.0 as u64,
            },
            _ => return Err(Missing),
        };
        Ok(h)
    }

    fn adt(&self, _: Seal, d: Def) -> Fetch<AdtDef<'_>> {
        let def = match d {
            ADT_POS => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_I32_I32,
                variants: VAR_2,
                sized_tail: Some(Ty(0)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_I32_I32,
                digest: d.0 as u64,
            },
            ADT_UNIT | ADT_WRAPPER => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_EMPTY,
                variants: VAR_0,
                sized_tail: None,
                discr: Discr::Unread,
                where_clauses: &[],
                portable: Portable::EMPTY,
                digest: d.0 as u64,
            },
            ADT_MIXED => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_I32_BOOL,
                variants: VAR_2,
                sized_tail: Some(Ty(1)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_I32_BOOL,
                digest: d.0 as u64,
            },
            ADT_BAD => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_I32_F32,
                variants: VAR_2,
                sized_tail: Some(Ty(1)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_I32_F32,
                digest: d.0 as u64,
            },
            ADT_SHIP => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_SHIP,
                variants: VAR_2,
                sized_tail: Some(Ty(1)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_SHIP,
                digest: d.0 as u64,
            },
            ADT_BAD_SHIP => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_BAD_SHIP,
                variants: VAR_2,
                sized_tail: Some(Ty(1)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_BAD_SHIP,
                digest: d.0 as u64,
            },
            ADT_PAIR => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 1,
                fields: FIELDS_PAIR,
                variants: VAR_2,
                sized_tail: Some(Ty(0)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_PAIR,
                digest: d.0 as u64,
            },
            ADT_ENUM => AdtDef {
                flags: AdtFlags(AdtFlags::ENUM),
                generics: 0,
                fields: FIELDS_I32_BOOL,
                variants: &[(0, 1), (1, 1)],
                sized_tail: None,
                discr: Discr::Int(IntTy::Isize),
                where_clauses: &[],
                portable: PORT_I32_BOOL,
                digest: d.0 as u64,
            },
            ADT_UNION => AdtDef {
                flags: AdtFlags(AdtFlags::UNION),
                generics: 0,
                fields: FIELDS_I32_BOOL,
                variants: VAR_2,
                sized_tail: None,
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_I32_BOOL,
                digest: d.0 as u64,
            },
            ADT_SELF_REC => AdtDef {
                flags: AdtFlags(AdtFlags::STRUCT),
                generics: 0,
                fields: FIELDS_SELF_REC,
                variants: VAR_1,
                sized_tail: Some(Ty(0)),
                discr: Discr::Unread,
                where_clauses: &[],
                portable: PORT_SELF_REC,
                digest: d.0 as u64,
            },
            ADT_MISSING => return Err(Missing),
            _ => return Err(Missing),
        };
        Ok(def)
    }

    fn fn_sig(&self, _: Seal, _: Def) -> Fetch<FnSigDef<'_>> {
        Err(Missing)
    }

    fn assoc(&self, _: Seal, d: Def) -> Fetch<AssocDef> {
        if d == ASSOC_OUT {
            Ok(AssocDef {
                container: TR_ASSOC,
                trait_: Some(TR_ASSOC),
                kind: AssocKind::Type,
                parent_generics: 1,
                own_generics: 0,
                digest: d.0 as u64,
            })
        } else {
            Err(Missing)
        }
    }

    fn opaque(&self, _: Seal, _: Def) -> Fetch<OpaqueDef<'_>> {
        Err(Missing)
    }

    fn lang(&self, _: Seal, _: Lang) -> Fetch<Option<Def>> {
        Ok(None)
    }

    fn is_local(&self, _: Seal, _: Def) -> Fetch<bool> {
        Ok(true)
    }

    fn const_holds(&self, _: Seal, _: Def, _: &[Ty]) -> Fetch<ConstHolds> {
        Err(Missing)
    }

    fn specializes(&self, _: Seal, _: Def, _: Def) -> Fetch<bool> {
        Ok(false)
    }
}

fn eval_pred(world: &World, tys: &mut TestTys, clauses: &[Clause], pred: Pred) -> Result<Answer, Stop> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut solver = Solver::new_in(alloc);
    let mut reads: OwnVec<Read, _> = OwnVec::new_in(alloc);
    let env = solver.env(tys, world, clauses, &mut reads)?;
    solver.evaluate(tys, world, TypingMode::Analysis, Goal { env, pred, bound: 0 }, &mut reads)
}

#[test]
fn pred_layout_and_canon_roundtrip() {
    assert!(size_of::<Pred>() <= 32, "Pred grew past 32 bytes: {}", size_of::<Pred>());

    let alloc = blaze_own::testing::heap().alloc();
    let mut tys = TestTys::new();
    let container = tys.mk(TyKind::Adt(ADT_POS, List::EMPTY)).expect("mk adt");
    let u8_ty = tys.mk(TyKind::Uint(UintTy::U8)).expect("mk u8");
    let extra = tys.mk_list(&[u8_ty]).expect("mk list");
    let pred = Pred::ForFields { container, trait_: TR_TAG, args_after_self: extra };

    let mut canon = Canon::new_in(alloc);
    let mut words = OwnVec::new_in(alloc);
    canon.pred(&tys, &mut words, pred).expect("canon pred");

    let mut dec = Decode::new_in(alloc);
    let decoded = dec.run(&mut tys, &words).expect("decode pred").expect("some pred");
    assert_eq!(decoded, pred);
}

#[test]
fn for_fields_concrete_structs_enums_unions() {
    let world = World::new();
    let mut tys = TestTys::new();
    let i32_ty = tys.mk(TyKind::Int(IntTy::I32)).expect("i32");
    let f32_ty = tys.mk(TyKind::Float(FloatTy::F32)).expect("f32");

    let pos = tys.mk(TyKind::Adt(ADT_POS, List::EMPTY)).expect("Pos");
    let unit = tys.mk(TyKind::Adt(ADT_UNIT, List::EMPTY)).expect("Unit");
    let mixed = tys.mk(TyKind::Adt(ADT_MIXED, List::EMPTY)).expect("Mixed");
    let bad = tys.mk(TyKind::Adt(ADT_BAD, List::EMPTY)).expect("Bad");
    let i32_list = tys.mk_list(&[i32_ty]).expect("l");
    let f32_list = tys.mk_list(&[f32_ty]).expect("l");
    let pair_i32 = tys.mk(TyKind::Adt(ADT_PAIR, i32_list)).expect("Pair<i32>");
    let pair_f32 = tys.mk(TyKind::Adt(ADT_PAIR, f32_list)).expect("Pair<f32>");
    let enum_ty = tys.mk(TyKind::Adt(ADT_ENUM, List::EMPTY)).expect("Enum");
    let union_ty = tys.mk(TyKind::Adt(ADT_UNION, List::EMPTY)).expect("Union");

    let ask = |tys: &mut TestTys, container: Ty| {
        eval_pred(&world, tys, &[], Pred::ForFields { container, trait_: TR_CODEC, args_after_self: List::EMPTY })
            .expect("eval")
    };

    assert_eq!(ask(&mut tys, pos), Answer::Yes);
    assert_eq!(ask(&mut tys, unit), Answer::Yes);
    assert_eq!(ask(&mut tys, mixed), Answer::Yes);
    assert_eq!(ask(&mut tys, pair_i32), Answer::Yes);
    assert_eq!(ask(&mut tys, bad), Answer::No);
    assert_eq!(ask(&mut tys, pair_f32), Answer::No);
    // Enums and unions are NOT structs -> NoSolution.
    assert_eq!(ask(&mut tys, enum_ty), Answer::No);
    assert_eq!(ask(&mut tys, union_ty), Answer::No);
}

#[test]
fn for_fields_record_and_tuple() {
    let world = World::new();
    let mut tys = TestTys::new();
    let i32_ty = tys.mk(TyKind::Int(IntTy::I32)).expect("i32");
    let bool_ty = tys.mk(TyKind::Bool).expect("bool");
    let f32_ty = tys.mk(TyKind::Float(FloatTy::F32)).expect("f32");

    let good_list = tys.mk_list(&[i32_ty, bool_ty]).expect("good list");
    let bad_list = tys.mk_list(&[i32_ty, f32_ty]).expect("bad list");

    let rec_good = tys.mk(TyKind::Record(0x1234, good_list)).expect("rec good");
    let rec_bad = tys.mk(TyKind::Record(0x1234, bad_list)).expect("rec bad");
    let rec_empty = tys.mk(TyKind::Record(0x0, List::EMPTY)).expect("rec empty");

    let tup_good = tys.mk(TyKind::Tuple(good_list)).expect("tup good");
    let tup_bad = tys.mk(TyKind::Tuple(bad_list)).expect("tup bad");
    let tup_empty = tys.mk(TyKind::Tuple(List::EMPTY)).expect("tup empty");

    let tagged_union = tys.mk(TyKind::Union(good_list)).expect("structural union");

    let ask = |tys: &mut TestTys, container: Ty| {
        eval_pred(&world, tys, &[], Pred::ForFields { container, trait_: TR_CODEC, args_after_self: List::EMPTY })
            .expect("eval")
    };

    assert_eq!(ask(&mut tys, rec_good), Answer::Yes);
    assert_eq!(ask(&mut tys, rec_bad), Answer::No);
    assert_eq!(ask(&mut tys, rec_empty), Answer::Yes);
    assert_eq!(ask(&mut tys, tup_good), Answer::Yes);
    assert_eq!(ask(&mut tys, tup_bad), Answer::No);
    assert_eq!(ask(&mut tys, tup_empty), Answer::Yes);
    assert_eq!(ask(&mut tys, tagged_union), Answer::No);
}

#[test]
fn for_fields_primitives_and_non_products_fail() {
    let world = World::new();
    let mut tys = TestTys::new();
    let i32_ty = tys.mk(TyKind::Int(IntTy::I32)).expect("i32");
    let bool_ty = tys.mk(TyKind::Bool).expect("bool");
    let f64_ty = tys.mk(TyKind::Float(FloatTy::F64)).expect("f64");
    let re = tys.mk(TyKind::ReStatic).expect("re");
    let ref_ty = tys.mk(TyKind::Ref(re, i32_ty, Mutbl::Not)).expect("&i32");
    let slice_ty = tys.mk(TyKind::Slice(i32_ty)).expect("[i32]");
    let len4 = tys.mk(TyKind::ConstValue(ValTy::USIZE, 4)).expect("4");
    let arr_ty = tys.mk(TyKind::Array(i32_ty, len4)).expect("[i32; 4]");

    for container in [i32_ty, bool_ty, f64_ty, ref_ty, slice_ty, arr_ty] {
        let ans = eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval");
        assert_eq!(ans, Answer::No, "expected No for non-product {container:?}");
    }
}

#[test]
fn for_fields_param_env_and_infer_vars() {
    let world = World::new();
    let mut tys = TestTys::new();
    let param0 = tys.mk(TyKind::Param(0)).expect("Param(0)");
    let param1 = tys.mk(TyKind::Param(1)).expect("Param(1)");

    let assume_p0 = [Clause {
        pred: Pred::ForFields { container: param0, trait_: TR_CODEC, args_after_self: List::EMPTY },
        bound: 0,
    }];

    // Param(0) WITH assumption -> Yes; WITHOUT -> No; Param(1) with Param(0) assumed -> No.
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &assume_p0,
            Pred::ForFields { container: param0, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::Yes
    );
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: param0, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::No
    );
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &assume_p0,
            Pred::ForFields { container: param1, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::No
    );

    // Unresolved inference variable -> Ambiguous (even when a ParamEnv assumption exists!).
    let var_empty = tys.fresh(VarKind::Ty).expect("fresh");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: var_empty, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::Maybe(Certainty::Ambiguous)
    );
    let var_with_env = tys.fresh(VarKind::Ty).expect("fresh");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &assume_p0,
            Pred::ForFields { container: var_with_env, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::Maybe(Certainty::Ambiguous)
    );
    assert!(matches!(tys.kind(tys.shallow_resolve(var_with_env)), TyKind::Infer(_)));
}

#[test]
fn for_fields_with_args_after_self() {
    let world = World::new();
    let mut tys = TestTys::new();
    let mixed = tys.mk(TyKind::Adt(ADT_MIXED, List::EMPTY)).expect("Mixed");
    let param0 = tys.mk(TyKind::Param(0)).expect("Param(0)");
    let u8_ty = tys.mk(TyKind::Uint(UintTy::U8)).expect("u8");
    let u16_ty = tys.mk(TyKind::Uint(UintTy::U16)).expect("u16");
    let arg_u8 = tys.mk_list(&[u8_ty]).expect("[u8]");
    let arg_u16 = tys.mk_list(&[u16_ty]).expect("[u16]");

    assert_eq!(
        eval_pred(&world, &mut tys, &[], Pred::ForFields { container: mixed, trait_: TR_TAG, args_after_self: arg_u8 })
            .expect("eval"),
        Answer::Yes
    );
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: mixed, trait_: TR_TAG, args_after_self: arg_u16 }
        )
        .expect("eval"),
        Answer::No
    );

    // Inference variable in args_after_self gets unified across fields!
    let infer_arg = tys.fresh(VarKind::Ty).expect("fresh arg");
    let arg_infer = tys.mk_list(&[infer_arg]).expect("[?0]");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: mixed, trait_: TR_TAG, args_after_self: arg_infer }
        )
        .expect("eval"),
        Answer::Yes
    );
    assert_eq!(tys.shallow_resolve(infer_arg), u8_ty);

    // ParamEnv matching with args_after_self.
    let env_tag = [Clause {
        pred: Pred::ForFields { container: param0, trait_: TR_TAG, args_after_self: arg_u8 },
        bound: 0,
    }];
    let infer_env = tys.fresh(VarKind::Ty).expect("fresh env arg");
    let arg_infer_env = tys.mk_list(&[infer_env]).expect("[?1]");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &env_tag,
            Pred::ForFields { container: param0, trait_: TR_TAG, args_after_self: arg_infer_env }
        )
        .expect("eval"),
        Answer::Yes
    );
    assert_eq!(tys.shallow_resolve(infer_env), u8_ty);
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &env_tag,
            Pred::ForFields { container: param0, trait_: TR_TAG, args_after_self: arg_u16 }
        )
        .expect("eval"),
        Answer::No
    );
}

#[test]
fn for_fields_recursive_blanket_impl_and_coherence() {
    let world = World::new();
    let mut tys = TestTys::new();
    let i32_ty = tys.mk(TyKind::Int(IntTy::I32)).expect("i32");
    let bool_ty = tys.mk(TyKind::Bool).expect("bool");
    let f32_ty = tys.mk(TyKind::Float(FloatTy::F32)).expect("f32");
    let ship = tys.mk(TyKind::Adt(ADT_SHIP, List::EMPTY)).expect("Ship");
    let bad_ship = tys.mk(TyKind::Adt(ADT_BAD_SHIP, List::EMPTY)).expect("BadShip");
    let enum_ty = tys.mk(TyKind::Adt(ADT_ENUM, List::EMPTY)).expect("Enum");
    let rec_nested = {
        let pos = tys.mk(TyKind::Adt(ADT_POS, List::EMPTY)).expect("Pos");
        let l = tys.mk_list(&[pos, bool_ty]).expect("l");
        tys.mk(TyKind::Record(0x9999, l)).expect("{ pos: Pos, ok: bool }")
    };

    let prove_codec = |tys: &mut TestTys, ty: Ty| {
        let args = tys.mk_list(&[ty]).expect("args");
        eval_pred(&world, tys, &[], Pred::Trait { trait_: TR_CODEC, args, polarity: Polarity::Positive })
            .expect("eval")
    };

    // Nested struct Ship { pos: Pos { i32, i32 }, ok: bool }: FieldCodec -> Yes!
    assert_eq!(prove_codec(&mut tys, ship), Answer::Yes);
    // Nested structural record { pos: Pos, ok: bool }: FieldCodec -> Yes!
    assert_eq!(prove_codec(&mut tys, rec_nested), Answer::Yes);
    // Primitives with explicit impls -> Yes (NO ambiguity with blanket impl!).
    assert_eq!(prove_codec(&mut tys, i32_ty), Answer::Yes);
    assert_eq!(prove_codec(&mut tys, bool_ty), Answer::Yes);
    // Nested bad struct, unimplemented primitive, enum -> No!
    assert_eq!(prove_codec(&mut tys, bad_ship), Answer::No);
    assert_eq!(prove_codec(&mut tys, f32_ty), Answer::No);
    assert_eq!(prove_codec(&mut tys, enum_ty), Answer::No);

    // Coherence: blanket impl is DISJOINT from `impl FieldCodec for i32`,
    // and OVERLAPS with `impl FieldCodec for Pos`!
    let alloc = blaze_own::testing::heap().alloc();
    let mut solver = Solver::new_in(alloc);
    let mut reads: OwnVec<Read, _> = OwnVec::new_in(alloc);
    assert_eq!(
        overlap(&mut solver, &mut tys, &world, &mut reads, IMPL_CODEC_BLANKET, IMPL_CODEC_I32).expect("overlap"),
        Overlap::Disjoint
    );
    assert_eq!(
        overlap(&mut solver, &mut tys, &world, &mut reads, IMPL_CODEC_BLANKET, IMPL_CODEC_POS_EXPLICIT)
            .expect("overlap"),
        Overlap::Overlap
    );
}

#[test]
fn for_fields_cycles_normalization_and_missing_adt() {
    let world = World::new();
    let mut tys = TestTys::new();

    // Recursive struct `struct SelfRec { next: SelfRec }`:
    // - Inductive trait: hits cycle protection without infinite loop (Overflow).
    // - Coinductive trait: resolves coinductively (Yes).
    let self_rec = tys.mk(TyKind::Adt(ADT_SELF_REC, List::EMPTY)).expect("SelfRec");
    let self_rec_args = tys.mk_list(&[self_rec]).expect("l");
    let rec_codec = eval_pred(
        &world,
        &mut tys,
        &[],
        Pred::Trait { trait_: TR_CODEC, args: self_rec_args, polarity: Polarity::Positive },
    )
    .expect("inductive cycle terminates");
    assert_eq!(rec_codec, Answer::Maybe(Certainty::Overflow));

    let rec_coind = eval_pred(
        &world,
        &mut tys,
        &[],
        Pred::Trait {
            trait_: TR_COINDUCTIVE,
            args: self_rec_args,
            polarity: Polarity::Positive,
        },
    )
    .expect("coinductive cycle terminates");
    assert_eq!(rec_coind, Answer::Yes);

    // Alias container normalization: `<Wrapper as Assoc>::Out` normalizes to `Pos` -> Yes.
    let wrapper = tys.mk(TyKind::Adt(ADT_WRAPPER, List::EMPTY)).expect("Wrapper");
    let wrapper_args = tys.mk_list(&[wrapper]).expect("l");
    let alias_out = tys
        .mk(TyKind::Alias(Rigid::No, AliasKind::Projection, ASSOC_OUT, wrapper_args))
        .expect("<Wrapper as Assoc>::Out");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: alias_out, trait_: TR_CODEC, args_after_self: List::EMPTY },
        )
        .expect("eval"),
        Answer::Yes
    );

    // Missing ADT fact propagates `Stop::Need(Question::Adt(ADT_MISSING))`.
    let missing_adt = tys.mk(TyKind::Adt(ADT_MISSING, List::EMPTY)).expect("Missing");
    assert_eq!(
        eval_pred(
            &world,
            &mut tys,
            &[],
            Pred::ForFields { container: missing_adt, trait_: TR_CODEC, args_after_self: List::EMPTY },
        ),
        Err(Stop::Need(Question::Adt(ADT_MISSING)))
    );
}
