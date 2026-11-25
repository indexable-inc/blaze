//! The evaluator's tests: bodies in blaze-body's text form, run over a table world (the old
//! evaluator's fixture world for ADTs and layout facts, plus a table of bodies). Each test
//! prints the value or the error it checks; the line it should print is in its doc.

use std::format;
use std::println;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_body::{AssertKind, BinOp, Body, Stmt, verify};
use blaze_build::layout::ClosureFacts;
use blaze_build::type_name::TypeNames;
use blaze_build::{FnTrait, instance_key};
use blaze_consteval::fixture::World;
use blaze_consteval::{
    Answer, ConstFn, ConstRecord, ConstVtable, ConstWorld, DataRecord, DataTarget, FnKind, FnTraitMethod,
    Intrinsic, LayoutFacts, Layouts, Limits, Missing, NeedKind, Op, RecordKey, Refusal, ReprAttrs, ValidRange,
};
use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_own::testing::heap;
use blaze_typeck::{
    Adt, AdtKind, CtorKind, ExprId, Fetch, Field, FnPtrHeader, ItemKey, List, Mutbl, Name, PNode, PTy,
    PortableTys, Variant,
};

use crate::{Bodied, BodyWorld, DropGlue, EvalError, Fault, eval_body};

const ROOT: ItemKey = ItemKey(0xb0d1);
const CALLEE: ItemKey = ItemKey(0xb0d2);

struct CustomAdt {
    key: ItemKey,
    kind: AdtKind,
    variants: StdVec<Variant>,
    fields: StdVec<Field>,
    repr: ReprAttrs,
}

/// The fixture world, plus bodies in the text form by item.
struct Tw {
    w: World,
    bodies: StdVec<(ItemKey, Body<Alloc<'static>>)>,
    promoted_bodies: StdVec<(ItemKey, ExprId, Body<Alloc<'static>>)>,
    promoted_records: StdVec<(ItemKey, ExprId, ConstRecord<Alloc<'static>>)>,
    anon_bodies: StdVec<(ItemKey, ExprId, Body<Alloc<'static>>)>,
    anon_records: StdVec<(ItemKey, ExprId, ConstRecord<Alloc<'static>>)>,
    extra_records: StdVec<(RecordKey, ConstRecord<Alloc<'static>>)>,
    mut_statics: StdVec<ItemKey>,
    closures: StdVec<(ItemKey, u32, PTy, PTy)>,
    custom_adts: StdVec<CustomAdt>,
    extra_intrinsics: StdVec<(ItemKey, Intrinsic)>,
    drop_adts: StdVec<ItemKey>,
    fn_once_item: Option<ItemKey>,
    type_names_on: bool,
}

/// The types the bodies name, as `tN` handles of the world's table.
struct Tys {
    unit: u32,
    u8: u32,
    u32: u32,
    u64: u32,
    i32: u32,
    usize: u32,
    bool: u32,
    char: u32,
    str_ref: u32,
    pair: u32,
    p: u32,
    r: u32,
    e: u32,
    isize: u32,
    arr4: u32,
}

fn world() -> (Tw, Tys) {
    let mut w = World::new();
    let b = w.fx.ty(PNode::Bool);
    let unit = w.fx.tuple(&[]);
    w.intrinsic("assume", 0, &[b], unit, Intrinsic::Assume);
    w.strukt("P", &[("a", "u32"), ("b", "u32")]);
    w.enum_("E", None, &[("A", &[], None), ("B", &["u32"], None)]);
    w.build();
    let mut t = |s: &str| -> u32 { w.ty(s).0 };
    let tys = Tys {
        unit: unit.0,
        u8: t("u8"),
        u32: t("u32"),
        u64: t("u64"),
        i32: t("i32"),
        usize: t("usize"),
        bool: t("bool"),
        char: t("char"),
        str_ref: t("&str"),
        pair: t("(u32, bool)"),
        p: t("P"),
        r: t("&u32"),
        e: t("E"),
        isize: t("isize"),
        arr4: t("[u32; 4]"),
    };
    (
        Tw {
            w,
            bodies: StdVec::new(),
            promoted_bodies: StdVec::new(),
            promoted_records: StdVec::new(),
            anon_bodies: StdVec::new(),
            anon_records: StdVec::new(),
            extra_records: StdVec::new(),
            mut_statics: StdVec::new(),
            closures: StdVec::new(),
            custom_adts: StdVec::new(),
            extra_intrinsics: StdVec::new(),
            drop_adts: StdVec::new(),
            fn_once_item: None,
            type_names_on: false,
        },
        tys,
    )
}

fn parse_verified(text: &str) -> Body<Alloc<'static>> {
    let body = blaze_body::parse(text, heap().alloc()).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"));
    if let Err(e) = verify(&body) {
        panic!("verify refused {e:?}:\n{text}");
    }
    body
}

impl Tw {
    fn with(mut self, item: ItemKey, text: &str) -> Tw {
        self.bodies.push((item, parse_verified(text)));
        self
    }

    fn with_promoted_body(mut self, owner: ItemKey, borrow: ExprId, text: &str) -> Tw {
        self.promoted_bodies.push((owner, borrow, parse_verified(text)));
        self
    }

    fn with_promoted_record(mut self, owner: ItemKey, borrow: ExprId, rec: ConstRecord<Alloc<'static>>) -> Tw {
        self.promoted_records.push((owner, borrow, rec));
        self
    }

    fn with_anon_body(mut self, owner: ItemKey, block: ExprId, text: &str) -> Tw {
        self.anon_bodies.push((owner, block, parse_verified(text)));
        self
    }

    fn with_anon_record(mut self, owner: ItemKey, block: ExprId, rec: ConstRecord<Alloc<'static>>) -> Tw {
        self.anon_records.push((owner, block, rec));
        self
    }

    fn with_record(mut self, item: ItemKey, data: DataRecord<Alloc<'static>>, fns: &[ConstFn], vtables: &[ConstVtable]) -> Tw {
        let alloc = heap().alloc();
        let g = PortableTys::new_in(alloc);
        let inst = instance_key(self.w.code_id(item), List::EMPTY, g.view(), alloc).expect("inst key");
        let key = RecordKey::of(inst);
        let mut fn_vec = OwnVec::new_in(alloc);
        for &f in fns {
            fn_vec.push(f);
        }
        fn_vec.sort_by_key(|f| f.key);
        let mut vt_vec = OwnVec::new_in(alloc);
        for &v in vtables {
            vt_vec.push(v);
        }
        vt_vec.sort_by_key(|v| v.key);
        let rec = ConstRecord { key, data, reads: OwnVec::new_in(alloc), fns: fn_vec, vtables: vt_vec };
        self.extra_records.push((key, rec));
        self
    }

    fn with_mut_static(mut self, item: ItemKey) -> Tw {
        self.mut_statics.push(item);
        self
    }

    fn with_intrinsic(mut self, key: ItemKey, intr: Intrinsic) -> Tw {
        self.extra_intrinsics.push((key, intr));
        self
    }

    fn with_closure(mut self, owner: ItemKey, ordinal: u32, sig: PTy, env: PTy) -> Tw {
        self.closures.push((owner, ordinal, sig, env));
        self
    }

    fn with_union(mut self, key: ItemKey, fields: &[(&str, PTy)]) -> Tw {
        let mut fs = StdVec::new();
        for &(name, ty) in fields {
            fs.push(Field { name: Name::of(name), ty });
        }
        let vs = StdVec::from([Variant {
            key,
            name: Name::of("U"),
            ctor: CtorKind::Braces,
            fields: List { start: 0, len: fs.len() as u32 },
        }]);
        self.custom_adts.push(CustomAdt { key, kind: AdtKind::Union, variants: vs, fields: fs, repr: ReprAttrs::RUST });
        self
    }

    fn with_empty_enum(mut self, key: ItemKey) -> Tw {
        self.custom_adts.push(CustomAdt {
            key,
            kind: AdtKind::Enum,
            variants: StdVec::new(),
            fields: StdVec::new(),
            repr: ReprAttrs::RUST,
        });
        self
    }

    fn with_aligned_struct(mut self, key: ItemKey, align_log2: u8, fields: &[(&str, PTy)]) -> Tw {
        let mut fs = StdVec::new();
        for &(name, ty) in fields {
            fs.push(Field { name: Name::of(name), ty });
        }
        let vs = StdVec::from([Variant {
            key,
            name: Name::of("Aligned"),
            ctor: CtorKind::Braces,
            fields: List { start: 0, len: fs.len() as u32 },
        }]);
        let mut repr = ReprAttrs::RUST;
        repr.c = true;
        repr.align = Some(align_log2);
        self.custom_adts.push(CustomAdt { key, kind: AdtKind::Struct, variants: vs, fields: fs, repr });
        self
    }
}

impl TypeNames for Tw {
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool {
        if item == self.w.key("P") {
            out("crate::P");
            return true;
        }
        if item == self.w.key("E") {
            out("crate::E");
            return true;
        }
        false
    }

    fn variant_name(&self, adt: ItemKey, variant: u32, out: &mut dyn FnMut(&str)) -> bool {
        if adt == self.w.key("E") {
            match variant {
                0 => {
                    out("A");
                    return true;
                }
                1 => {
                    out("B");
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

impl LayoutFacts for Tw {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        if let Some(c) = self.custom_adts.iter().find(|c| c.key == key) {
            return Some(Adt {
                kind: c.kind,
                generics: 0,
                variants: &c.variants,
                fields: &c.fields,
                types: self.w.fx.t.view(),
            });
        }
        self.w.adt(key)
    }
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        if let Some(c) = self.custom_adts.iter().find(|c| c.key == key) {
            return Ok(c.repr);
        }
        self.w.repr(key)
    }
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        if self.custom_adts.iter().any(|c| c.key == key) {
            return Ok(None);
        }
        self.w.discriminant(key, variant)
    }
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        if self.custom_adts.iter().any(|c| c.key == key) {
            return Ok(ValidRange::default());
        }
        self.w.valid_range(key)
    }
    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<ClosureFacts<'_>> {
        self.closures.iter().find(|&&(o, ord, _, _)| o == owner && ord == ordinal).map(|&(_, _, sig, env)| {
            ClosureFacts { types: self.w.fx.t.view(), sig, env, kind: FnTrait::Fn }
        })
    }
}

impl ConstWorld for Tw {
    fn body(&self, item: ItemKey) -> Fetch<Option<blaze_consteval::Typed<'_>>> {
        self.w.body(item)
    }
    /// The table's bodies are `const fn`s; the rest as the fixture says.
    fn fn_kind(&self, item: ItemKey) -> FnKind {
        if let Some(&(_, intr)) = self.extra_intrinsics.iter().find(|&&(k, _)| k == item) {
            return FnKind::Intrinsic(intr);
        }
        if self.bodies.iter().any(|(k, _)| *k == item) { FnKind::Const } else { self.w.fn_kind(item) }
    }
    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Option<(ItemKey, List)> {
        self.w.resolve(item, args, ground, scratch)
    }
    fn code_id(&self, item: ItemKey) -> ContentId {
        self.w.code_id(item)
    }
    fn is_static(&self, item: ItemKey) -> bool {
        self.w.is_static(item)
    }
    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        if let Some((_, rec)) = self.extra_records.iter().find(|(k, _)| *k == key) {
            return Ok(rec);
        }
        self.w.record(key)
    }
    fn explicit_discriminant(&self, adt: ItemKey, variant: u32) -> Option<ItemKey> {
        self.w.explicit_discriminant(adt, variant)
    }
    fn fn_trait_method(&self, which: FnTrait) -> FnTraitMethod {
        if which == FnTrait::FnOnce
            && let Some(item) = self.fn_once_item
        {
            return FnTraitMethod::Found(item);
        }
        self.w.fn_trait_method(which)
    }
    fn type_names(&self) -> Option<&dyn TypeNames> {
        if self.type_names_on { Some(self) } else { None }
    }
}

impl BodyWorld for Tw {
    fn verified(&self, item: ItemKey) -> Fetch<Option<Bodied<'_>>> {
        Ok(self.bodies.iter().find(|(k, _)| *k == item).map(|(k, b)| Bodied {
            item: *k,
            body: verify(b).expect("verified when added"),
            types: self.w.fx.t.view(),
        }))
    }
    fn drop_glue(&self, ty: PTy, ground: &mut PortableTys<'_>, _scratch: Alloc<'_>) -> Result<DropGlue, Refusal<'static>> {
        if let Ok(PNode::Adt(key, _)) = ground.view().node(ty)
            && self.drop_adts.contains(&key)
        {
            Ok(DropGlue::Call { item: CALLEE, args: List::EMPTY })
        } else {
            Ok(DropGlue::Nothing)
        }
    }
    fn promoted(&self, owner: ItemKey, borrow: ExprId) -> Fetch<Option<Bodied<'_>>> {
        Ok(self
            .promoted_bodies
            .iter()
            .find(|(o, b, _)| *o == owner && *b == borrow)
            .map(|(o, _, body)| Bodied { item: *o, body: verify(body).expect("verified"), types: self.w.fx.t.view() }))
    }
    fn promoted_record(&self, owner: ItemKey, borrow: ExprId, _args: &[PTy]) -> Fetch<Option<&ConstRecord<Alloc<'_>>>> {
        Ok(self.promoted_records.iter().find(|(o, b, _)| *o == owner && *b == borrow).map(|(_, _, r)| r))
    }
    fn anon(&self, owner: ItemKey, block: ExprId) -> Fetch<Option<Bodied<'_>>> {
        Ok(self
            .anon_bodies
            .iter()
            .find(|(o, b, _)| *o == owner && *b == block)
            .map(|(o, _, body)| Bodied { item: *o, body: verify(body).expect("verified"), types: self.w.fx.t.view() }))
    }
    fn anon_record(&self, owner: ItemKey, block: ExprId, _args: &[PTy]) -> Fetch<Option<&ConstRecord<Alloc<'_>>>> {
        Ok(self.anon_records.iter().find(|(o, b, _)| *o == owner && *b == block).map(|(_, _, r)| r))
    }
    fn is_mut_static(&self, item: ItemKey) -> bool {
        self.mut_statics.contains(&item)
    }
}

fn eval_ans(tw: &Tw, limits: Limits) -> Result<Answer<DataRecord<Alloc<'static>>>, EvalError<'static>> {
    let alloc = heap().alloc();
    let mut layouts = Layouts::new_in(alloc);
    let mut ground = PortableTys::new_in(alloc);
    eval_body(tw, &mut layouts, &mut ground, ROOT, List::EMPTY, limits, alloc)
}

fn run(tw: &Tw, limits: Limits) -> Result<DataRecord<Alloc<'static>>, EvalError<'static>> {
    match eval_ans(tw, limits)? {
        Answer::Done(r) => Ok(r),
        Answer::Need(n) => panic!("the table world lacks {n:?}"),
    }
}

fn make_u32_record(val: u32) -> ConstRecord<Alloc<'static>> {
    let (tw, t) = world();
    let text = format!(
        "local _0: t{u} return\nconst c0 = scalar 0x{val:x}: t{u}\nbb0:\n  _0 = const c0\n  return\n",
        u = t.u32,
    );
    let tw = tw.with(ROOT, &text);
    let data = run(&tw, Limits::DEFAULT).expect("make_u32_record");
    let alloc = heap().alloc();
    ConstRecord {
        key: RecordKey(ContentId::of("test-u32", &val.to_le_bytes())),
        data,
        reads: OwnVec::new_in(alloc),
        fns: OwnVec::new_in(alloc),
        vtables: OwnVec::new_in(alloc),
    }
}

/// The value's first four bytes as a `u32`, printed.
fn u32_value(tw: &Tw) -> u32 {
    let r = run(tw, Limits::DEFAULT).unwrap_or_else(|e| panic!("refused: {e}"));
    let b = &r.allocs[0].bytes;
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    println!("{v}");
    v
}

fn refused(tw: &Tw, limits: Limits) -> EvalError<'static> {
    match run(tw, limits) {
        Ok(_) => panic!("evaluated"),
        Err(e) => {
            println!("{e}");
            e
        }
    }
}

fn checked_add(t: &Tys, a: &str, b: &str) -> String {
    format!(
        "\
local _0: t{u} return
local _1: t{pair} temp e0 value
const c0 = scalar {a}: t{u}
const c1 = scalar {b}: t{u}
bb0:
  _1 = checked add(const c0, const c1)
  assert copy _1.1:t{bool} == false, overflow_add(const c0, const c1) then bb1 unwind continue
bb1:
  _0 = copy _1.0:t{u}
  return
",
        u = t.u32,
        pair = t.pair,
        bool = t.bool,
    )
}

/// `2 + 3` through `checked add` and its overflow assert. Should print: `5`
#[test]
fn checked_add_passes() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &checked_add(&t, "0x2", "0x3"));
    assert_eq!(u32_value(&tw), 5);
}

/// `u32::MAX + 1`: the assert fails with the overflow refusal at its row. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 1, bytes 0..0): assert Overflow(Add) failed: Overflow { op: Add }`
#[test]
fn checked_add_overflows() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &checked_add(&t, "0xffffffff", "0x1"));
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Assert { kind: AssertKind::Overflow(BinOp::Add), refusal: Refusal::Overflow { op: Op::Add } }), "{e:?}");
    assert_eq!(e.at.map(|r| (r.body, r.stmt)), Some((ROOT, Stmt(1))));
}

/// The sum of `0..10` through a switch and a back edge. Should print: `45`
#[test]
fn loop_sums() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value mut
local _2: t{b} temp e1 value
const c0 = scalar 0x0: t{u}
const c1 = scalar 0x1: t{u}
const c2 = scalar 0xa: t{u}
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c2)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _0 = add(copy _0, copy _1)
  _1 = add(copy _1, const c1)
  goto bb1
bb3:
  return
",
        u = t.u32,
        b = t.bool,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 45);
}

/// A call to a second body (`x + 7`) with `35`, its return value the constant's. Should
/// print: `42`
#[test]
fn call_returns() {
    let (tw, t) = world();
    let callee = format!(
        "\
local _0: t{u} return
local _1: t{u} arg 0
const c0 = scalar 0x7: t{u}
bb0:
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let root = format!(
        "\
local _0: t{u} return
const c0 = scalar 0x23: t{u}
bb0:
  call fn k{callee:x}<0,0>(const c0) -> _0 then bb1 unwind continue
bb1:
  return
",
        u = t.u32,
        callee = CALLEE.0,
    );
    let tw = tw.with(CALLEE, &callee).with(ROOT, &root);
    assert_eq!(u32_value(&tw), 42);
}

/// `P { a: 5, b: 9 }` built field by field; `a` read back plus `b` read through a shared
/// reference. Should print: `14`
#[test]
fn struct_fields_and_reference() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{p} temp e0 value mut
local _2: t{r} temp e1 borrow
local _3: t{u} temp e2 value
const c0 = scalar 0x5: t{u}
const c1 = scalar 0x9: t{u}
origin 'o0
bb0:
  _1.0:t{u} = const c0
  _1.1:t{u} = const c1
  _2 = borrow shared _1.1:t{u} L0 'o0
  _3 = copy _2.*
  _0 = add(copy _1.0:t{u}, copy _3)
  return
",
        u = t.u32,
        p = t.p,
        r = t.r,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 14);
}

/// `E::B(4)` built, its discriminant switched on, the payload read through a downcast.
/// Should print: `4`
#[test]
fn switch_on_discriminant() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{e} temp e0 value
local _2: t{i} temp e1 value
const c0 = scalar 0x4: t{u}
const c1 = scalar 0x1: t{u}
bb0:
  _1 = adt t{e}#1(const c0)
  _2 = discr _1
  switch copy _2 [0 -> bb1, 1 -> bb2] else bb3
bb1:
  _0 = const c1
  return
bb2:
  _0 = copy _1#1.0:t{u}
  return
bb3:
  unreachable
",
        u = t.u32,
        e = t.e,
        i = t.isize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 4);
}

/// `E::B(4)` built as MIR builds an enum field by field: `deinit`, the payload through a
/// downcast, then `setdiscr` writes the tag; the discriminant read back selects the payload.
/// Should print: `4`
#[test]
fn set_discriminant_after_fields() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{e} temp e0 value mut
local _2: t{i} temp e1 value
const c0 = scalar 0x4: t{u}
const c1 = scalar 0x1: t{u}
bb0:
  deinit _1
  _1#1.0:t{u} = const c0
  setdiscr _1 variant 1
  _2 = discr _1
  switch copy _2 [0 -> bb1, 1 -> bb2] else bb3
bb1:
  _0 = const c1
  return
bb2:
  _0 = copy _1#1.0:t{u}
  return
bb3:
  unreachable
",
        u = t.u32,
        e = t.e,
        i = t.isize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 4);
}

/// A local written, then `deinit`: its bytes are unwritten again, so reading it is refused.
/// Should print: `constant 0xb0d1 (body 0xb0d1, row 2, bytes 0..0): Uninit { offset: 0 }`
#[test]
fn deinit_unwrites_the_place() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{u} temp e0 value mut
const c0 = scalar 0x4: t{u}
bb0:
  _1 = const c0
  deinit _1
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let tw = tw.with(ROOT, &text);
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Uninit { offset: 0 })), "{e:?}");
    assert_eq!(e.at.map(|r| r.stmt), Some(Stmt(2)));
}

/// An `unreachable` row reached: undefined behaviour, refused at its row. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 0, bytes 0..0): Unreachable`
#[test]
fn unreachable_is_refused() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &format!("local _0: t{u} return\nbb0:\n  unreachable\n", u = t.u32));
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Unreachable)), "{e:?}");
    assert_eq!(e.at.map(|r| r.stmt), Some(Stmt(0)));
}

/// `loop {}`: the step bound stops it by name. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 0, bytes 0..0): StepLimit { limit: 1000 }`
#[test]
fn step_limit_stops_a_loop() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &format!("local _0: t{u} return\nbb0:\n  goto bb0\n", u = t.u32));
    let e = refused(&tw, Limits { steps: 1000, ..Limits::DEFAULT });
    assert!(matches!(e.fault, Fault::Refused(Refusal::StepLimit { limit: 1000 })), "{e:?}");
}

/// `Intrinsic::Assume(false)` is UB and must be refused with `Refusal::Unreachable`, while
/// `Intrinsic::Assume(true)` succeeds.
#[test]
fn assume_false_is_refused_and_true_passes() {
    let (tw, t) = world();
    let assume = tw.w.key("assume").0;
    let body_false = format!(
        "\
local _0: t{u} return
local _1: t{unit} temp e0 value
const c0 = scalar 0x0: t{b}
const c1 = scalar 0x7: t{u}
bb0:
  call intrinsic k{assume:x}<0,0>(const c0) -> _1 then bb1 unwind continue
bb1:
  _0 = const c1
  return
",
        u = t.u32,
        unit = t.unit,
        b = t.bool,
    );
    let tw_false = tw.with(ROOT, &body_false);
    let e = refused(&tw_false, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Unreachable)), "{e:?}");

    let (tw, t) = world();
    let assume = tw.w.key("assume").0;
    let body_true = format!(
        "\
local _0: t{u} return
local _1: t{unit} temp e0 value
const c0 = scalar 0x1: t{b}
const c1 = scalar 0x7: t{u}
bb0:
  call intrinsic k{assume:x}<0,0>(const c0) -> _1 then bb1 unwind continue
bb1:
  _0 = const c1
  return
",
        u = t.u32,
        unit = t.unit,
        b = t.bool,
    );
    let tw_true = tw.with(ROOT, &body_true);
    assert_eq!(u32_value(&tw_true), 7);
}

/// `UnOp::PtrMetadata` on a `&str` bytes constant (`ConstKind::Bytes`) must return its length
/// and not leak `Origin::Data` allocations in a loop.
#[test]
fn ptr_metadata_on_bytes_const_and_loop_no_leak() {
    let (tw, t) = world();
    // 64 bytes ("aa" * 64 = 128 hex chars), looped 200 times under a 1024-byte memory limit
    // (would allocate 12.8 KiB and fail with MemoryLimit if ConstKind::Bytes leaked).
    let hex = "61".repeat(64);
    let text = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value mut
local _2: t{b} temp e1 value
local _3: t{us} temp e2 value
const c0 = scalar 0x0: t{u}
const c1 = scalar 0x1: t{u}
const c2 = scalar 0xc8: t{u}
const c3 = bytes \"{hex}\": t{s}
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c2)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _3 = meta(const c3)
  _0 = cast int_to_int(copy _3) as t{u}
  _1 = add(copy _1, const c1)
  goto bb1
bb3:
  return
",
        u = t.u32,
        b = t.bool,
        us = t.usize,
        s = t.str_ref,
    );
    let tw = tw.with(ROOT, &text);
    let r = run(&tw, Limits { memory: 1024, ..Limits::DEFAULT }).unwrap_or_else(|e| panic!("refused: {e}"));
    let bytes = &r.allocs[0].bytes;
    assert_eq!(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]), 64);
}

/// Invalid `bool` (> 1) and invalid `char` (surrogate `0xD800`) values must be refused on
/// scalar read / transmute.
#[test]
fn invalid_bool_and_char_are_refused() {
    let (tw, t) = world();
    let bad_bool = format!(
        "\
local _0: t{b} return
const c0 = scalar 0x2: t{u8}
bb0:
  _0 = cast transmute(const c0) as t{b}
  return
",
        b = t.bool,
        u8 = t.u8,
    );
    let tw_b = tw.with(ROOT, &bad_bool);
    let e = refused(&tw_b, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::InvalidBool { value: 2 })), "{e:?}");

    let (tw, t) = world();
    let bad_char = format!(
        "\
local _0: t{ch} return
const c0 = scalar 0xd800: t{u}
bb0:
  _0 = cast transmute(const c0) as t{ch}
  return
",
        ch = t.char,
        u = t.u32,
    );
    let tw_c = tw.with(ROOT, &bad_char);
    let e = refused(&tw_c, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::InvalidChar { value: 0xD800 })), "{e:?}");
}

/// `Value::Repeat` evaluates its operand once and writes all elements.
#[test]
fn repeat_hoisted_builds_array() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{arr} temp e0 value
const c0 = scalar 0x9: t{u}
const c1 = scalar 0x4: t{us}
bb0:
  _1 = repeat(const c0, c1)
  _0 = add(copy _1[0 of 4], copy _1[3 of 4])
  return
",
        u = t.u32,
        arr = t.arr4,
        us = t.usize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 18);
}

/// `ConstKind::Promoted` executes via `BodyWorld::promoted` (cached across a loop) and falls back
/// to `BodyWorld::promoted_record` when no body is provided.
#[test]
fn promoted_const_body_and_record() {
    let (tw, t) = world();
    let promoted_body = format!(
        "\
local _0: t{u} return
const c0 = scalar 0x15: t{u}
const c1 = scalar 0x15: t{u}
bb0:
  _0 = add(const c0, const c1)
  return
",
        u = t.u32,
    );
    let root = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value mut
local _2: t{b} temp e1 value
local _3: t{r} temp e2 value
const c0 = scalar 0x0: t{u}
const c1 = scalar 0x1: t{u}
const c2 = scalar 0xa: t{u}
const c3 = promoted 7: t{r}
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c2)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _3 = const c3
  _0 = add(copy _0, copy _3.*)
  _1 = add(copy _1, const c1)
  goto bb1
bb3:
  return
",
        u = t.u32,
        b = t.bool,
        r = t.r,
    );
    let tw = tw.with_promoted_body(ROOT, ExprId(7), &promoted_body).with(ROOT, &root);
    assert_eq!(u32_value(&tw), 420);

    // Fallback to `promoted_record` when `promoted` returns `Ok(None)`.
    let (tw, t) = world();
    let root_rec = format!(
        "\
local _0: t{u} return
local _1: t{r} temp e0 value
const c0 = promoted 9: t{r}
bb0:
  _1 = const c0
  _0 = copy _1.*
  return
",
        u = t.u32,
        r = t.r,
    );
    let tw = tw.with_promoted_record(ROOT, ExprId(9), make_u32_record(99)).with(ROOT, &root_rec);
    assert_eq!(u32_value(&tw), 99);
}

/// `ConstKind::Anon` executes an inline `const { .. }` body (including nested `const fn` calls)
/// and falls back to `BodyWorld::anon_record`.
#[test]
fn anon_const_body_and_record() {
    let (tw, t) = world();
    let callee = format!(
        "\
local _0: t{u} return
local _1: t{u} arg 0
const c0 = scalar 0x3: t{u}
bb0:
  _0 = mul(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let anon_body = format!(
        "\
local _0: t{u} return
const c0 = scalar 0xe: t{u}
bb0:
  call fn k{callee:x}<0,0>(const c0) -> _0 then bb1 unwind continue
bb1:
  return
",
        u = t.u32,
        callee = CALLEE.0,
    );
    let root = format!(
        "\
local _0: t{u} return
const c0 = anon e11: t{u}
bb0:
  _0 = add(const c0, const c0)
  return
",
        u = t.u32,
    );
    let tw = tw.with(CALLEE, &callee).with_anon_body(ROOT, ExprId(11), &anon_body).with(ROOT, &root);
    assert_eq!(u32_value(&tw), 84);

    // Fallback to `anon_record`.
    let (tw, t) = world();
    let root_rec = format!(
        "\
local _0: t{u} return
const c0 = anon e12: t{u}
bb0:
  _0 = const c0
  return
",
        u = t.u32,
    );
    let tw = tw.with_anon_record(ROOT, ExprId(12), make_u32_record(123)).with(ROOT, &root_rec);
    assert_eq!(u32_value(&tw), 123);
}

/// `ConstKind::StaticAddr` and `DataTarget::Static`: reading an immutable static, returning a
/// static reference (`DataTarget::Static`), requesting `NeedKind::Value` when unloaded, refusing
/// reads of `static mut`, and decoding a constant record that points to a static.
#[test]
fn static_addr_and_data_target_static() {
    const STAT: ItemKey = ItemKey(0x57a1);
    const STAT_MUT: ItemKey = ItemKey(0x57a2);
    const CONST_REF_STAT: ItemKey = ItemKey(0x57a3);

    let (mut tw, t) = world();
    let mut_ptr_u32 = tw.w.ty("*mut u32").0;
    let stat_data = make_u32_record(77).data;

    // 1. Reading an immutable static with a loaded record.
    let read_stat = format!(
        "\
local _0: t{u} return
local _1: t{r} temp e0 value
const c0 = static k{s:x}: t{r}
bb0:
  _1 = const c0
  _0 = copy _1.*
  return
",
        u = t.u32,
        r = t.r,
        s = STAT.0,
    );
    let tw_loaded = tw.with_record(STAT, stat_data, &[], &[]).with(ROOT, &read_stat);
    assert_eq!(u32_value(&tw_loaded), 77);

    // 2. Taking `&STAT` when STAT is NOT loaded still succeeds and emits `DataTarget::Static(STAT)`.
    let (tw2, t2) = world();
    let take_stat = format!(
        "\
local _0: t{r} return
const c0 = static k{s:x}: t{r}
bb0:
  _0 = const c0
  return
",
        r = t2.r,
        s = STAT.0,
    );
    let tw_ref = tw2.with(ROOT, &take_stat);
    let rec_ref_stat = run(&tw_ref, Limits::DEFAULT).expect("taking static address succeeds");
    assert_eq!(rec_ref_stat.allocs[0].relocs.len(), 1);
    assert_eq!(rec_ref_stat.allocs[0].relocs[0].target, DataTarget::Static(STAT));

    // 3. Reading unloaded STAT requests `Answer::Need(NeedKind::Value)`.
    let (tw3, _) = world();
    let tw_unloaded = tw3.with(ROOT, &read_stat);
    match eval_ans(&tw_unloaded, Limits::DEFAULT).expect("need") {
        Answer::Need(n) => {
            assert_eq!(n.kind, NeedKind::Value);
            assert_eq!(n.item, STAT);
        }
        Answer::Done(_) => panic!("expected Need for unloaded static"),
    }

    // 4. `static mut`: taking address succeeds with `DataTarget::Static`, reading is refused.
    let (mut tw4, _) = world();
    let mp = tw4.w.ty("*mut u32").0;
    let take_mut = format!(
        "\
local _0: t{mp} return
const c0 = static k{sm:x}: t{mp}
bb0:
  _0 = const c0
  return
",
        sm = STAT_MUT.0,
    );
    let tw_mut_addr = tw4.with_mut_static(STAT_MUT).with(ROOT, &take_mut);
    let rec_mut = run(&tw_mut_addr, Limits::DEFAULT).expect("mut static addr");
    assert_eq!(rec_mut.allocs[0].relocs[0].target, DataTarget::Static(STAT_MUT));

    let (mut tw5, t5) = world();
    let mp5 = tw5.w.ty("*mut u32").0;
    let _ = mut_ptr_u32;
    let read_mut = format!(
        "\
local _0: t{u} return
local _1: t{mp5} temp e0 value
const c0 = static k{sm:x}: t{mp5}
bb0:
  _1 = const c0
  _0 = copy _1.*
  return
",
        u = t5.u32,
        sm = STAT_MUT.0,
    );
    let tw_mut_read = tw5.with_mut_static(STAT_MUT).with(ROOT, &read_mut);
    let err = refused(&tw_mut_read, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::MutableStatic { item }) if item == STAT_MUT), "{err:?}");

    // 5. `DataTarget::Static` inside a `ConstKind::Item` record decodes and reads through cleanly.
    let (tw6, t6) = world();
    let read_const_stat = format!(
        "\
local _0: t{u} return
local _1: t{r} temp e0 value
const c0 = item k{c:x}<0,0>: t{r}
bb0:
  _1 = const c0
  _0 = copy _1.*
  return
",
        u = t6.u32,
        r = t6.r,
        c = CONST_REF_STAT.0,
    );
    let tw_chain = tw6
        .with_record(STAT, make_u32_record(314).data, &[], &[])
        .with_record(CONST_REF_STAT, rec_ref_stat, &[], &[])
        .with(ROOT, &read_const_stat);
    assert_eq!(u32_value(&tw_chain), 314);
}

/// `CastKind::Unsize` to `&dyn Trait`, trait object upcasting (`&dyn Sub -> &dyn Super`),
/// reborrowing `&*dyn_ref`, `DataTarget::Vtable` record decoding/interning, and
/// `size_of_val` / `align_of_val` on `&dyn Trait`.
#[test]
fn unsize_to_dyn_vtable_upcast_and_size_of_val() {
    const TR_SUB: ItemKey = ItemKey(0x7a01);
    const TR_SUPER: ItemKey = ItemKey(0x7a02);
    const CONST_DYN: ItemKey = ItemKey(0x7a03);
    const INTR_SOV: ItemKey = ItemKey(0x7a10);
    const INTR_AOV: ItemKey = ItemKey(0x7a11);

    let (mut tw, t) = world();
    let empty = tw.w.fx.t.intern_list(&[]).unwrap();
    let ex_sub = tw.w.fx.t.intern(PNode::ExTrait(TR_SUB, empty)).unwrap();
    let preds_sub = tw.w.fx.t.intern_list(&[ex_sub]).unwrap();
    let dyn_sub = tw.w.fx.t.intern(PNode::Dynamic(preds_sub)).unwrap();
    let ref_dyn_sub = tw.w.fx.t.intern(PNode::Ref(Mutbl::Not, dyn_sub)).unwrap().0;

    let ex_super = tw.w.fx.t.intern(PNode::ExTrait(TR_SUPER, empty)).unwrap();
    let preds_super = tw.w.fx.t.intern_list(&[ex_super]).unwrap();
    let dyn_super = tw.w.fx.t.intern(PNode::Dynamic(preds_super)).unwrap();
    let ref_dyn_super = tw.w.fx.t.intern(PNode::Ref(Mutbl::Not, dyn_super)).unwrap().0;

    let promoted_u32 = make_u32_record(88);

    // 1. Unsize `&u32` (from promoted) to `&dyn Sub`, reborrow `&*d`, return it -> `DataTarget::Vtable`.
    let body_sub = format!(
        "\
local _0: t{ref_dyn_sub} return
local _1: t{r} temp e0 value
local _2: t{ref_dyn_sub} temp e1 value
const c0 = promoted 1: t{r}
origin 'o0
bb0:
  _1 = const c0
  _2 = cast unsize(copy _1) as t{ref_dyn_sub}
  _0 = borrow shared _2.* L0 'o0
  return
",
        r = t.r,
    );
    let tw_sub = tw.with_promoted_record(ROOT, ExprId(1), promoted_u32).with(ROOT, &body_sub);
    let rec_sub = run(&tw_sub, Limits::DEFAULT).expect("unsize to dyn Sub");
    assert_eq!(rec_sub.allocs[0].relocs.len(), 2);
    let DataTarget::Vtable(vk_sub) = rec_sub.allocs[0].relocs[1].target else {
        panic!("expected DataTarget::Vtable reloc, got {:?}", rec_sub.allocs[0].relocs);
    };

    // 2. Unsize `&u32 -> &dyn Sub -> &dyn Super` upcast produces a distinct `DataTarget::Vtable`.
    let body_upcast = format!(
        "\
local _0: t{ref_dyn_super} return
local _1: t{r} temp e0 value
local _2: t{ref_dyn_sub} temp e1 value
const c0 = promoted 2: t{r}
bb0:
  _1 = const c0
  _2 = cast unsize(copy _1) as t{ref_dyn_sub}
  _0 = cast unsize(copy _2) as t{ref_dyn_super}
  return
",
        r = t.r,
    );
    let mut tw_up = tw_sub;
    tw_up.bodies.clear();
    tw_up = tw_up.with_promoted_record(ROOT, ExprId(2), make_u32_record(88)).with(ROOT, &body_upcast);
    let rec_super = run(&tw_up, Limits::DEFAULT).expect("upcast dyn Sub -> dyn Super");
    let DataTarget::Vtable(vk_super) = rec_super.allocs[0].relocs[1].target else {
        panic!("expected super Vtable reloc");
    };
    assert_ne!(vk_sub, vk_super);

    // 3. `size_of_val` + `align_of_val` on `&dyn Sub` returns concrete `u32` size (4) + align (4) = 8.
    let body_sov = format!(
        "\
local _0: t{u} return
local _1: t{r} temp e0 value
local _2: t{ref_dyn_sub} temp e1 value
local _3: t{us} temp e2 value
local _4: t{us} temp e3 value
local _5: t{us} temp e4 value
const c0 = promoted 2: t{r}
bb0:
  _1 = const c0
  _2 = cast unsize(copy _1) as t{ref_dyn_sub}
  call intrinsic k{sov:x}<0,0>(copy _2) -> _3 then bb1 unwind continue
bb1:
  call intrinsic k{aov:x}<0,0>(copy _2) -> _4 then bb2 unwind continue
bb2:
  _5 = add(copy _3, copy _4)
  _0 = cast int_to_int(copy _5) as t{u}
  return
",
        u = t.u32,
        r = t.r,
        us = t.usize,
        sov = INTR_SOV.0,
        aov = INTR_AOV.0,
    );
    let mut tw_sov = tw_up;
    tw_sov.bodies.clear();
    tw_sov = tw_sov
        .with_intrinsic(INTR_SOV, Intrinsic::SizeOfVal)
        .with_intrinsic(INTR_AOV, Intrinsic::AlignOfVal)
        .with(ROOT, &body_sov);
    assert_eq!(u32_value(&tw_sov), 8);

    // 4. Decoding a `ConstRecord` with `DataTarget::Vtable(vk_sub)` preserves `vk_sub` on re-intern.
    let body_const_dyn = format!(
        "\
local _0: t{ref_dyn_sub} return
const c0 = item k{c:x}<0,0>: t{ref_dyn_sub}
bb0:
  _0 = const c0
  return
",
        c = CONST_DYN.0,
    );
    let mut tw_dec = tw_sov;
    tw_dec.bodies.clear();
    tw_dec = tw_dec.with_record(CONST_DYN, rec_sub, &[], &[]).with(ROOT, &body_const_dyn);
    let rec_re = run(&tw_dec, Limits::DEFAULT).expect("decode vtable const");
    assert_eq!(rec_re.allocs[0].relocs[1].target, DataTarget::Vtable(vk_sub));
}

/// `AggregateKind::Closure`, `Proj::Field` on closures, `CastKind::ClosureFnPointer`, and calling
/// a reified fn pointer via `Callee::Value`.
#[test]
fn closure_aggregate_fn_pointer_and_indirect_call() {
    const CALL_ONCE: ItemKey = ItemKey(0xc100);

    let (mut tw, t) = world();
    let u32_ty = PTy(t.u32);
    let sig_list = tw.w.fx.t.intern_list(&[u32_ty, u32_ty]).unwrap();
    let fn_ptr_ty = tw.w.fx.t.intern(PNode::FnPtr(sig_list, FnPtrHeader::RUST)).unwrap();
    let empty_list = tw.w.fx.t.intern_list(&[]).unwrap();
    let empty_env = tw.w.fx.t.intern(PNode::Tuple(empty_list)).unwrap();
    let cap_list = tw.w.fx.t.intern_list(&[u32_ty, u32_ty]).unwrap();
    let cap_env = tw.w.fx.t.intern(PNode::Tuple(cap_list)).unwrap();

    let non_cap_closure = tw.w.fx.t.intern(PNode::Closure { owner: ROOT, ordinal: 0, args: empty_list }).unwrap().0;
    let cap_closure = tw.w.fx.t.intern(PNode::Closure { owner: ROOT, ordinal: 1, args: empty_list }).unwrap().0;

    // 1. Capturing closure built via `AggregateKind::Closure`, fields read via `Proj::Field`.
    let body_cap = format!(
        "\
local _0: t{u} return
local _1: t{cap_closure} temp e0 value
const c0 = scalar 0x10: t{u}
const c1 = scalar 0x1a: t{u}
bb0:
  _1 = closure t{cap_closure}(const c0, const c1)
  _0 = add(copy _1.0:t{u}, copy _1.1:t{u})
  return
",
        u = t.u32,
    );
    tw = tw
        .with_closure(ROOT, 0, fn_ptr_ty, empty_env)
        .with_closure(ROOT, 1, fn_ptr_ty, cap_env);
    tw.fn_once_item = Some(CALL_ONCE);
    let tw_cap = tw.with(ROOT, &body_cap);
    assert_eq!(u32_value(&tw_cap), 42);

    // 2. Non-capturing closure coerced via `CastKind::ClosureFnPointer` -> `DataTarget::Fn(_)`.
    let body_cfp = format!(
        "\
local _0: t{fp} return
local _1: t{non_cap_closure} temp e0 value
bb0:
  _1 = closure t{non_cap_closure}()
  _0 = cast closure_fn_pointer(move _1) as t{fp}
  return
",
        fp = fn_ptr_ty.0,
    );
    let mut tw_cfp = tw_cap;
    tw_cfp.bodies.clear();
    tw_cfp = tw_cfp.with(ROOT, &body_cfp);
    let rec_cfp = run(&tw_cfp, Limits::DEFAULT).expect("closure_fn_pointer");
    assert_eq!(rec_cfp.allocs[0].relocs.len(), 1);
    assert!(matches!(rec_cfp.allocs[0].relocs[0].target, DataTarget::Fn(_)));

    // 3. Capturing closure cast via `CastKind::ClosureFnPointer` is refused.
    let body_bad_cfp = format!(
        "\
local _0: t{fp} return
local _1: t{cap_closure} temp e0 value
const c0 = scalar 0x1: t{u}
const c1 = scalar 0x2: t{u}
bb0:
  _1 = closure t{cap_closure}(const c0, const c1)
  _0 = cast closure_fn_pointer(move _1) as t{fp}
  return
",
        fp = fn_ptr_ty.0,
        u = t.u32,
    );
    let mut tw_bad = tw_cfp;
    tw_bad.bodies.clear();
    tw_bad = tw_bad.with(ROOT, &body_bad_cfp);
    let err = refused(&tw_bad, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::Unsupported { .. })), "{err:?}");

    // 4. `CastKind::ReifyFnPointer` + indirect call through fn pointer (`Callee::Value`).
    let fn_def_ty = tw_bad.w.fx.t.intern(PNode::FnDef(CALLEE, empty_list)).unwrap().0;
    let callee = format!(
        "\
local _0: t{u} return
local _1: t{u} arg 0
const c0 = scalar 0x9: t{u}
bb0:
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let body_call = format!(
        "\
local _0: t{u} return
local _1: t{fp} temp e0 value
const c0 = fn k{callee:x}<0,0>: t{fn_def_ty}
const c1 = scalar 0x21: t{u}
bb0:
  _1 = cast reify_fn_pointer(const c0) as t{fp}
  call value copy _1(const c1) -> _0 then bb1 unwind continue
bb1:
  return
",
        u = t.u32,
        fp = fn_ptr_ty.0,
        callee = CALLEE.0,
    );
    let mut tw_call = tw_bad;
    tw_call.bodies.clear();
    tw_call = tw_call.with(CALLEE, &callee).with(ROOT, &body_call);
    assert_eq!(u32_value(&tw_call), 42);
}

/// `AdtKind::Union` aggregates (active field write + trailing byte deinit), `PNode::Union`
/// discriminant/downcast, and `CastKind::Transmute` (pair <-> u64 + PointerAsInt refusal).
#[test]
fn union_structural_union_and_transmute() {
    const UNION_KEY: ItemKey = ItemKey(0x0011);

    let (mut tw, t) = world();
    let u8_ty = PTy(t.u8);
    let u32_ty = PTy(t.u32);
    let empty = tw.w.fx.t.intern_list(&[]).unwrap();
    let union_ty = tw.w.fx.t.intern(PNode::Adt(UNION_KEY, empty)).unwrap().0;
    tw = tw.with_union(UNION_KEY, &[("small", u8_ty), ("big", u32_ty)]);

    // 1. Write `big = 0x12345678`, read `small` -> `0x78`.
    let body_union_ok = format!(
        "\
local _0: t{u} return
local _1: t{union_ty} temp e0 value mut
local _2: t{u8} temp e1 value
const c0 = scalar 0x12345678: t{u}
bb0:
  _1 = adt t{union_ty}#1(const c0)
  _2 = copy _1.0:t{u8}
  _0 = cast int_to_int(copy _2) as t{u}
  return
",
        u = t.u32,
        u8 = t.u8,
    );
    let tw_uok = tw.with(ROOT, &body_union_ok);
    assert_eq!(u32_value(&tw_uok), 0x78);

    // 2. Write `big`, then overwrite with `small`: trailing bytes 1..4 become uninit -> reading `big` fails!
    let body_union_uninit = format!(
        "\
local _0: t{u} return
local _1: t{union_ty} temp e0 value mut
const c0 = scalar 0x12345678: t{u}
const c1 = scalar 0xaa: t{u8}
bb0:
  _1 = adt t{union_ty}#1(const c0)
  _1 = adt t{union_ty}#0(const c1)
  _0 = copy _1.1:t{u}
  return
",
        u = t.u32,
        u8 = t.u8,
    );
    let mut tw_uuninit = tw_uok;
    tw_uuninit.bodies.clear();
    tw_uuninit = tw_uuninit.with(ROOT, &body_union_uninit);
    let err = refused(&tw_uuninit, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::Uninit { offset: 1 })), "{err:?}");

    // 3. Structural tagged union `PNode::Union([u32, u8])`: `discr` + downcast.
    let members = tw_uuninit.w.fx.t.intern_list(&[u32_ty, u8_ty]).unwrap();
    let su_ty = tw_uuninit.w.fx.t.intern(PNode::Union(members)).unwrap().0;
    let body_su = format!(
        "\
local _0: t{u} return
local _1: t{su_ty} temp e0 value
local _2: t{i} temp e1 value
const c0 = scalar 0x55: t{u8}
bb0:
  _1 = adt t{su_ty}#1(const c0)
  _2 = discr _1
  switch copy _2 [0 -> bb1, 1 -> bb2] else bb3
bb1:
  _0 = copy _1#0.0:t{u}
  return
bb2:
  _0 = cast int_to_int(copy _1#1.0:t{u8}) as t{u}
  return
bb3:
  unreachable
",
        u = t.u32,
        u8 = t.u8,
        i = t.isize,
    );
    let mut tw_su = tw_uuninit;
    tw_su.bodies.clear();
    tw_su = tw_su.with(ROOT, &body_su);
    assert_eq!(u32_value(&tw_su), 0x55);

    // 4. `CastKind::Transmute`: `(u32, u32) -> u64`, and `&u32 -> usize` refused with `PointerAsInt`.
    let pair_u32 = tw_su.w.ty("(u32, u32)").0;
    let body_tr_pair = format!(
        "\
local _0: t{u} return
local _1: t{pair_u32} temp e0 value
local _2: t{u64} temp e1 value
local _3: t{u64} temp e2 value
const c0 = scalar 0x5: t{u}
const c1 = scalar 0x3: t{u}
const c2 = scalar 0x20: t{u}
bb0:
  _1 = tuple t{pair_u32}(const c0, const c1)
  _2 = cast transmute(copy _1) as t{u64}
  _3 = shr(copy _2, const c2)
  _0 = cast int_to_int(copy _3) as t{u}
  return
",
        u = t.u32,
        u64 = t.u64,
    );
    let mut tw_tr = tw_su;
    tw_tr.bodies.clear();
    tw_tr = tw_tr.with(ROOT, &body_tr_pair);
    assert_eq!(u32_value(&tw_tr), 3);

    let body_tr_ptr = format!(
        "\
local _0: t{us} return
local _1: t{u} temp e0 value
local _2: t{r} temp e1 borrow
const c0 = scalar 0x1: t{u}
origin 'o0
bb0:
  _1 = const c0
  _2 = borrow shared _1 L0 'o0
  _0 = cast transmute(copy _2) as t{us}
  return
",
        us = t.usize,
        u = t.u32,
        r = t.r,
    );
    let mut tw_ptr = tw_tr;
    tw_ptr.bodies.clear();
    tw_ptr = tw_ptr.with(ROOT, &body_tr_ptr);
    let err = refused(&tw_ptr, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::PointerAsInt)), "{err:?}");
}

/// `Proj::ConstIndex` `from_end`, `Proj::Subslice` (`from_end` true and false on both slices
/// and arrays), and out-of-bounds `from_end` refusal without host panic.
#[test]
fn slice_subslice_and_const_index_from_end() {
    let (mut tw, t) = world();
    let ref_arr4 = tw.w.ty("&[u32; 4]").0;
    let ref_slice = tw.w.ty("&[u32]").0;
    let arr2 = tw.w.ty("[u32; 2]").0;

    let body_clean = format!(
        "\
local _0: t{u} return
local _1: t{arr4} temp e0 value
local _2: t{ref_arr4} temp e1 borrow
local _3: t{ref_slice} temp e2 value
local _4: t{ref_slice} temp e3 borrow
local _5: t{arr2} temp e4 value
local _6: t{u} temp e5 value
local _7: t{u} temp e6 value mut
local _8: t{us} temp e7 value
local _9: t{u} temp e8 value
const c0 = scalar 0xa: t{u}
const c1 = scalar 0x14: t{u}
const c2 = scalar 0x1e: t{u}
const c3 = scalar 0x28: t{u}
origin 'o0
origin 'o1
bb0:
  _1 = array t{arr4}(const c0, const c1, const c2, const c3)
  _2 = borrow shared _1 L0 'o0
  _3 = cast unsize(copy _2) as t{ref_slice}
  _6 = copy _3.*[-1 of 2]
  _4 = borrow shared _3.*[1..-1] L1 'o1
  _8 = len _4.*
  _9 = cast int_to_int(copy _8) as t{u}
  _5 = copy _1[1..3]
  _7 = add(copy _6, copy _4.*[0 of 1])
  _7 = add(copy _7, copy _5[1 of 2])
  _0 = add(copy _7, copy _9)
  return
",
        u = t.u32,
        us = t.usize,
        arr4 = t.arr4,
    );
    let tw_ok = tw.with(ROOT, &body_clean);
    assert_eq!(u32_value(&tw_ok), 92);

    // Out-of-bounds `from_end` must return `IndexOutOfBounds`, never panic.
    let body_oob = format!(
        "\
local _0: t{u} return
local _1: t{arr4} temp e0 value
const c0 = scalar 0x1: t{u}
bb0:
  _1 = array t{arr4}(const c0, const c0, const c0, const c0)
  _0 = copy _1[-5 of 1]
  return
",
        u = t.u32,
        arr4 = t.arr4,
    );
    let mut tw_oob = tw_ok;
    tw_oob.bodies.clear();
    tw_oob = tw_oob.with(ROOT, &body_oob);
    let err = refused(&tw_oob, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::IndexOutOfBounds { index: 5, len: 4 })), "{err:?}");
}

/// Memory intrinsics: `write_bytes`, `copy`, `copy_nonoverlapping` (including overlap refusal),
/// `read_via_copy`, `write_via_move`, `raw_eq`, and `compare_bytes`.
#[test]
fn memory_intrinsics_write_copy_read_compare() {
    const I_WB: ItemKey = ItemKey(0x9001);
    const I_COPY: ItemKey = ItemKey(0x9002);
    const I_CNOV: ItemKey = ItemKey(0x9003);
    const I_RVC: ItemKey = ItemKey(0x9004);
    const I_WVM: ItemKey = ItemKey(0x9005);
    const I_REQ: ItemKey = ItemKey(0x9006);
    const I_CMP: ItemKey = ItemKey(0x9007);

    let (mut tw, t) = world();
    let ptr_mut_u8 = tw.w.ty("*mut u8").0;
    let ptr_const_u8 = tw.w.ty("*const u8").0;
    let ptr_mut_u32 = tw.w.ty("*mut u32").0;
    let ptr_const_u32 = tw.w.ty("*const u32").0;
    let arr4_u8 = tw.w.ty("[u8; 4]").0;
    let u8_list = tw.w.fx.t.intern_list(&[PTy(t.u8)]).unwrap();
    let u32_list = tw.w.fx.t.intern_list(&[PTy(t.u32)]).unwrap();

    // write_bytes(&mut a, 0x01, 4) -> 0x01010101; copy_nonoverlapping(a -> b, 4); read_via_copy(b) == 0x01010101
    // raw_eq(&a, &b) == true; write_via_move(&mut c, 5); compare_bytes(&a, &b, 4) == 0.
    let body = format!(
        "\
local _0: t{u} return
local _1: t{arr4_u8} temp e0 value mut
local _2: t{arr4_u8} temp e1 value mut
local _3: t{ptr_mut_u8} temp e2 value
local _4: t{ptr_mut_u8} temp e3 value
local _5: t{ptr_const_u8} temp e4 value
local _6: t{ptr_const_u32} temp e5 value
local _7: t{u} temp e6 value
local _8: t{b} temp e7 value
local _9: t{i32} temp e8 value
local _10: t{u} temp e9 value mut
local _11: t{ptr_mut_u32} temp e10 value
local _12: t{unit} temp e11 value
const c0 = scalar 0x1: t{u8}
const c1 = scalar 0x4: t{us}
const c2 = scalar 0x5: t{u}
bb0:
  _3 = raw mut _1[0 of 4]
  call intrinsic k{wb:x}<{u8l_s},{u8l_l}>(copy _3, const c0, const c1) -> _12 then bb1 unwind continue
bb1:
  _5 = cast mut_to_const(copy _3) as t{ptr_const_u8}
  _4 = raw mut _2[0 of 4]
  call intrinsic k{cnov:x}<{u8l_s},{u8l_l}>(copy _5, copy _4, const c1) -> _12 then bb2 unwind continue
bb2:
  _6 = cast ptr_to_ptr(copy _4) as t{ptr_const_u32}
  call intrinsic k{rvc:x}<{u32l_s},{u32l_l}>(copy _6) -> _7 then bb3 unwind continue
bb3:
  call intrinsic k{req:x}<{u32l_s},{u32l_l}>(copy _5, copy _6) -> _8 then bb4 unwind continue
bb4:
  call intrinsic k{cmp:x}<0,0>(copy _5, copy _6, const c1) -> _9 then bb5 unwind continue
bb5:
  _11 = raw mut _10
  call intrinsic k{wvm:x}<{u32l_s},{u32l_l}>(copy _11, const c2) -> _12 then bb6 unwind continue
bb6:
  switch copy _8 [1 -> bb7] else bb9
bb7:
  switch copy _9 [0 -> bb8] else bb9
bb8:
  _0 = add(copy _7, copy _10)
  return
bb9:
  unreachable
",
        u = t.u32,
        u8 = t.u8,
        us = t.usize,
        b = t.bool,
        i32 = t.i32,
        unit = t.unit,
        wb = I_WB.0,
        cnov = I_CNOV.0,
        rvc = I_RVC.0,
        wvm = I_WVM.0,
        req = I_REQ.0,
        cmp = I_CMP.0,
        u8l_s = u8_list.start,
        u8l_l = u8_list.len,
        u32l_s = u32_list.start,
        u32l_l = u32_list.len,
    );
    tw = tw
        .with_intrinsic(I_WB, Intrinsic::WriteBytes)
        .with_intrinsic(I_COPY, Intrinsic::Copy)
        .with_intrinsic(I_CNOV, Intrinsic::CopyNonoverlapping)
        .with_intrinsic(I_RVC, Intrinsic::ReadViaCopy)
        .with_intrinsic(I_WVM, Intrinsic::WriteViaMove)
        .with_intrinsic(I_REQ, Intrinsic::RawEq)
        .with_intrinsic(I_CMP, Intrinsic::CompareBytes)
        .with(ROOT, &body);
    assert_eq!(u32_value(&tw), 0x0101_0101 + 5);

    // Overlapping `copy_nonoverlapping` is refused as UB (`Refusal::Unreachable`), while `copy` succeeds.
    let body_overlap = format!(
        "\
local _0: t{u} return
local _1: t{arr4_u8} temp e0 value mut
local _2: t{ptr_mut_u8} temp e1 value
local _3: t{ptr_const_u8} temp e2 value
local _4: t{ptr_mut_u8} temp e3 value
local _5: t{unit} temp e4 value
const c0 = scalar 0x2: t{u8}
const c1 = scalar 0x4: t{us}
const c2 = scalar 0x3: t{us}
bb0:
  _2 = raw mut _1[0 of 4]
  call intrinsic k{wb:x}<{u8l_s},{u8l_l}>(copy _2, const c0, const c1) -> _5 then bb1 unwind continue
bb1:
  _3 = cast mut_to_const(copy _2) as t{ptr_const_u8}
  _4 = raw mut _1[1 of 4]
  call intrinsic k{cnov:x}<{u8l_s},{u8l_l}>(copy _3, copy _4, const c2) -> _5 then bb2 unwind continue
bb2:
  _0 = cast int_to_int(copy _1[3 of 4]) as t{u}
  return
",
        u = t.u32,
        u8 = t.u8,
        us = t.usize,
        unit = t.unit,
        wb = I_WB.0,
        cnov = I_CNOV.0,
        u8l_s = u8_list.start,
        u8l_l = u8_list.len,
    );
    let mut tw_ov = tw;
    tw_ov.bodies.clear();
    tw_ov = tw_ov.with(ROOT, &body_overlap);
    let err = refused(&tw_ov, Limits::DEFAULT);
    assert!(matches!(err.fault, Fault::Refused(Refusal::Unreachable)), "{err:?}");

    // Same overlapping range with `copy` (memmove) succeeds!
    let body_memmove = body_overlap.replace(&format!("k{:x}", I_CNOV.0), &format!("k{:x}", I_COPY.0));
    let mut tw_mm = tw_ov;
    tw_mm.bodies.clear();
    tw_mm = tw_mm.with(ROOT, &body_memmove);
    assert_eq!(u32_value(&tw_mm), 2);
}

/// Bit, saturating, unchecked, exact_div, validity (`assert_inhabited` / `assert_zero_valid`),
/// `const_eval_select`, `needs_drop`, `discriminant_value`, `type_id`, and `type_name` intrinsics.
#[test]
fn arithmetic_validity_reflection_and_select_intrinsics() {
    const I_CTPOP: ItemKey = ItemKey(0x9101);
    const I_CTLZ_NZ: ItemKey = ItemKey(0x9102);
    const I_BSWAP: ItemKey = ItemKey(0x9103);
    const I_ROTL: ItemKey = ItemKey(0x9104);
    const I_SAT_ADD: ItemKey = ItemKey(0x9105);
    const I_EXACT_DIV: ItemKey = ItemKey(0x9106);
    const I_INHAB: ItemKey = ItemKey(0x9107);
    const I_ZVALID: ItemKey = ItemKey(0x9108);
    const I_SELECT: ItemKey = ItemKey(0x9109);
    const I_NDROP: ItemKey = ItemKey(0x910a);
    const I_DISCR: ItemKey = ItemKey(0x910b);
    const I_TNAME: ItemKey = ItemKey(0x910c);
    const EMPTY_ENUM: ItemKey = ItemKey(0x91ff);

    let (mut tw, t) = world();
    let u32_ty = PTy(t.u32);
    let ref_u32_ty = PTy(t.r);
    let empty_list = tw.w.fx.t.intern_list(&[]).unwrap();
    let uninhab_ty = tw.w.fx.t.intern(PNode::Adt(EMPTY_ENUM, empty_list)).unwrap();
    let u32_list = tw.w.fx.t.intern_list(&[u32_ty]).unwrap();
    let r_list = tw.w.fx.t.intern_list(&[ref_u32_ty]).unwrap();
    let un_list = tw.w.fx.t.intern_list(&[uninhab_ty]).unwrap();
    let p_list = tw.w.fx.t.intern_list(&[PTy(t.p)]).unwrap();
    let ref_e = tw.w.ty("&E").0;
    let fn_def_callee = tw.w.fx.t.intern(PNode::FnDef(CALLEE, empty_list)).unwrap().0;
    let tup1_u32 = tw.w.fx.t.intern(PNode::Tuple(u32_list)).unwrap().0;

    let callee = format!(
        "\
local _0: t{u} return
local _1: t{u} arg 0
const c0 = scalar 0xa: t{u}
bb0:
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );

    // Run ctpop(0x0f)=4, ctlz_nonzero(0x80000000)=0, bswap(0x01000000)=1, rotl(1, 3)=8,
    // sat_add(u32::MAX, 5)=u32::MAX -> (wrapped+1=0), exact_div(20, 4)=5, assert_inhabited::<u32>(),
    // assert_zero_valid::<u32>(), const_eval_select((7,), callee, callee)=17, needs_drop::<P>()=1,
    // discriminant_value(&E::B(9))=1, type_name::<P>().len() == "crate::P".len() == 8.
    // Total sum: 4 + 1 + 8 + 5 + 17 + 1 + 1 + 8 = 45.
    let body_valid = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value
local _2: t{u} temp e1 value
local _3: t{u} temp e2 value
local _4: t{u} temp e3 value
local _5: t{u} temp e4 value
local _6: t{u} temp e5 value
local _7: t{unit} temp e6 value
local _8: t{tup1} temp e7 value
local _9: t{u} temp e8 value
local _10: t{b} temp e9 value
local _11: t{e} temp e10 value
local _12: t{ref_e} temp e11 borrow
local _13: t{i} temp e12 value
local _14: t{sr} temp e13 value
local _15: t{us} temp e14 value
local _16: t{u} temp e15 value
local _17: t{u} temp e16 value
local _18: t{u} temp e17 value
const c0 = scalar 0xf: t{u}
const c1 = scalar 0x80000000: t{u}
const c2 = scalar 0x01000000: t{u}
const c3 = scalar 0x1: t{u}
const c4 = scalar 0x3: t{u}
const c5 = scalar 0xffffffff: t{u}
const c6 = scalar 0x14: t{u}
const c7 = scalar 0x4: t{u}
const c8 = scalar 0x7: t{u}
const c9 = fn k{callee:x}<0,0>: t{fn_def_callee}
const c10 = scalar 0x9: t{u}
origin 'o0
bb0:
  call intrinsic k{ctpop:x}<{ul_s},{ul_l}>(const c0) -> _1 then bb1 unwind continue
bb1:
  call intrinsic k{ctlz:x}<{ul_s},{ul_l}>(const c1) -> _2 then bb2 unwind continue
bb2:
  call intrinsic k{bswap:x}<{ul_s},{ul_l}>(const c2) -> _3 then bb3 unwind continue
bb3:
  call intrinsic k{rotl:x}<{ul_s},{ul_l}>(const c3, const c4) -> _4 then bb4 unwind continue
bb4:
  call intrinsic k{sadd:x}<{ul_s},{ul_l}>(const c5, const c3) -> _5 then bb5 unwind continue
bb5:
  call intrinsic k{ediv:x}<{ul_s},{ul_l}>(const c6, const c7) -> _6 then bb6 unwind continue
bb6:
  call intrinsic k{inhab:x}<{ul_s},{ul_l}>() -> _7 then bb7 unwind continue
bb7:
  call intrinsic k{zv:x}<{ul_s},{ul_l}>() -> _7 then bb8 unwind continue
bb8:
  _8 = tuple t{tup1}(const c8)
  call intrinsic k{sel:x}<0,0>(copy _8, const c9, const c9) -> _9 then bb9 unwind continue
bb9:
  call intrinsic k{nd:x}<{pl_s},{pl_l}>() -> _10 then bb10 unwind continue
bb10:
  _11 = adt t{e}#1(const c10)
  _12 = borrow shared _11 L0 'o0
  call intrinsic k{discr:x}<0,0>(copy _12) -> _13 then bb11 unwind continue
bb11:
  call intrinsic k{tname:x}<{pl_s},{pl_l}>() -> _14 then bb12 unwind continue
bb12:
  _15 = meta(copy _14)
  _16 = cast int_to_int(copy _15) as t{u}
  _17 = cast int_to_int(copy _10) as t{u}
  _18 = cast int_to_int(copy _13) as t{u}
  switch copy _5 [4294967295 -> bb13] else bb14
bb13:
  _0 = add(copy _1, copy _2)
  _0 = add(copy _0, copy _3)
  _0 = add(copy _0, copy _4)
  _0 = add(copy _0, copy _6)
  _0 = add(copy _0, copy _9)
  _0 = add(copy _0, copy _17)
  _0 = add(copy _0, copy _18)
  _0 = add(copy _0, copy _16)
  return
bb14:
  unreachable
",
        u = t.u32,
        unit = t.unit,
        tup1 = tup1_u32,
        b = t.bool,
        e = t.e,
        i = t.isize,
        sr = t.str_ref,
        us = t.usize,
        callee = CALLEE.0,
        ctpop = I_CTPOP.0,
        ctlz = I_CTLZ_NZ.0,
        bswap = I_BSWAP.0,
        rotl = I_ROTL.0,
        sadd = I_SAT_ADD.0,
        ediv = I_EXACT_DIV.0,
        inhab = I_INHAB.0,
        zv = I_ZVALID.0,
        sel = I_SELECT.0,
        nd = I_NDROP.0,
        discr = I_DISCR.0,
        tname = I_TNAME.0,
        ul_s = u32_list.start,
        ul_l = u32_list.len,
        pl_s = p_list.start,
        pl_l = p_list.len,
    );
    tw = tw
        .with_empty_enum(EMPTY_ENUM)
        .with_intrinsic(I_CTPOP, Intrinsic::Ctpop)
        .with_intrinsic(I_CTLZ_NZ, Intrinsic::CtlzNonzero)
        .with_intrinsic(I_BSWAP, Intrinsic::Bswap)
        .with_intrinsic(I_ROTL, Intrinsic::RotateLeft)
        .with_intrinsic(I_SAT_ADD, Intrinsic::SaturatingAdd)
        .with_intrinsic(I_EXACT_DIV, Intrinsic::ExactDiv)
        .with_intrinsic(I_INHAB, Intrinsic::AssertInhabited)
        .with_intrinsic(I_ZVALID, Intrinsic::AssertZeroValid)
        .with_intrinsic(I_SELECT, Intrinsic::ConstEvalSelect)
        .with_intrinsic(I_NDROP, Intrinsic::NeedsDrop)
        .with_intrinsic(I_DISCR, Intrinsic::DiscriminantValue)
        .with_intrinsic(I_TNAME, Intrinsic::TypeName);
    let p_key = tw.w.key("P");
    tw.drop_adts.push(p_key);
    tw.type_names_on = true;
    tw = tw.with(CALLEE, &callee).with(ROOT, &body_valid);
    assert_eq!(u32_value(&tw), 45);

    // Negative checks: ctlz_nonzero(0), exact_div(7, 2), assert_inhabited::<Never>(), assert_zero_valid::<&u32>().
    let body_nz = format!(
        "local _0: t{u} return\nconst c0 = scalar 0x0: t{u}\nbb0:\n  call intrinsic k{ctlz:x}<{ul_s},{ul_l}>(const c0) -> _0 then bb1 unwind continue\nbb1:\n  return\n",
        u = t.u32,
        ctlz = I_CTLZ_NZ.0,
        ul_s = u32_list.start,
        ul_l = u32_list.len,
    );
    let mut tw_neg = tw;
    tw_neg.bodies.clear();
    tw_neg = tw_neg.with(ROOT, &body_nz);
    let e = refused(&tw_neg, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::NonzeroOnZero { name: "ctlz_nonzero" })), "{e:?}");

    let body_ediv = format!(
        "local _0: t{u} return\nconst c0 = scalar 0x7: t{u}\nconst c1 = scalar 0x2: t{u}\nbb0:\n  call intrinsic k{ediv:x}<{ul_s},{ul_l}>(const c0, const c1) -> _0 then bb1 unwind continue\nbb1:\n  return\n",
        u = t.u32,
        ediv = I_EXACT_DIV.0,
        ul_s = u32_list.start,
        ul_l = u32_list.len,
    );
    tw_neg.bodies.clear();
    tw_neg = tw_neg.with(ROOT, &body_ediv);
    let e = refused(&tw_neg, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Overflow { op: Op::Div })), "{e:?}");

    let body_uninhab = format!(
        "local _0: t{u} return\nlocal _1: t{unit} temp e0 value\nconst c0 = scalar 0x0: t{u}\nbb0:\n  call intrinsic k{inhab:x}<{s},{l}>() -> _1 then bb1 unwind continue\nbb1:\n  _0 = const c0\n  return\n",
        u = t.u32,
        unit = t.unit,
        inhab = I_INHAB.0,
        s = un_list.start,
        l = un_list.len,
    );
    tw_neg.bodies.clear();
    tw_neg = tw_neg.with(ROOT, &body_uninhab);
    let e = refused(&tw_neg, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Panic { .. })), "{e:?}");

    let body_zv = format!(
        "local _0: t{u} return\nlocal _1: t{unit} temp e0 value\nconst c0 = scalar 0x0: t{u}\nbb0:\n  call intrinsic k{zv:x}<{s},{l}>() -> _1 then bb1 unwind continue\nbb1:\n  _0 = const c0\n  return\n",
        u = t.u32,
        unit = t.unit,
        zv = I_ZVALID.0,
        s = r_list.start,
        l = r_list.len,
    );
    tw_neg.bodies.clear();
    tw_neg = tw_neg.with(ROOT, &body_zv);
    let e = refused(&tw_neg, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Panic { .. })), "{e:?}");
}

/// Pointer intrinsics: `offset`, `ptr_offset_from`, `ptr_guaranteed_cmp`, `aggregate_raw_ptr`,
/// `ptr_metadata`, and `slice_get_unchecked`.
#[test]
fn pointer_and_slice_intrinsics() {
    const I_OFF: ItemKey = ItemKey(0x9201);
    const I_OF: ItemKey = ItemKey(0x9202);
    const I_GCMP: ItemKey = ItemKey(0x9203);
    const I_AGGP: ItemKey = ItemKey(0x9204);
    const I_PMETA: ItemKey = ItemKey(0x9205);
    const I_SGET: ItemKey = ItemKey(0x9206);

    let (mut tw, t) = world();
    let ptr_const_u32 = tw.w.ty("*const u32").0;
    let ptr_const_slice = tw.w.ty("*const [u32]").0;

    let body = format!(
        "\
local _0: t{u} return mut
local _1: t{arr4} temp e0 value
local _2: t{pcu} temp e1 value
local _3: t{pcu} temp e2 value
local _4: t{i} temp e3 value
local _5: t{u8} temp e4 value
local _6: t{pcs} temp e5 value
local _7: t{us} temp e6 value
local _8: t{pcu} temp e7 value
local _9: t{u} temp e8 value
local _10: t{u} temp e9 value
local _11: t{u} temp e10 value
const c0 = scalar 0xa: t{u}
const c1 = scalar 0x14: t{u}
const c2 = scalar 0x1e: t{u}
const c3 = scalar 0x28: t{u}
const c4 = scalar 0x2: t{i}
const c5 = scalar 0x4: t{us}
const c6 = scalar 0x3: t{us}
bb0:
  _1 = array t{arr4}(const c0, const c1, const c2, const c3)
  _2 = raw const _1[0 of 4]
  call intrinsic k{off:x}<0,0>(copy _2, const c4) -> _3 then bb1 unwind continue
bb1:
  call intrinsic k{of:x}<0,0>(copy _3, copy _2) -> _4 then bb2 unwind continue
bb2:
  call intrinsic k{gcmp:x}<0,0>(copy _2, copy _3) -> _5 then bb3 unwind continue
bb3:
  call intrinsic k{aggp:x}<0,0>(copy _2, const c5) -> _6 then bb4 unwind continue
bb4:
  call intrinsic k{pmeta:x}<0,0>(copy _6) -> _7 then bb5 unwind continue
bb5:
  call intrinsic k{sget:x}<0,0>(copy _6, const c6) -> _8 then bb6 unwind continue
bb6:
  _9 = cast int_to_int(copy _4) as t{u}
  _10 = cast int_to_int(copy _5) as t{u}
  _11 = cast int_to_int(copy _7) as t{u}
  _0 = add(copy _3.*, copy _8.*)
  _0 = add(copy _0, copy _9)
  _0 = add(copy _0, copy _10)
  _0 = add(copy _0, copy _11)
  return
",
        u = t.u32,
        u8 = t.u8,
        i = t.isize,
        us = t.usize,
        arr4 = t.arr4,
        pcu = ptr_const_u32,
        pcs = ptr_const_slice,
        off = I_OFF.0,
        of = I_OF.0,
        gcmp = I_GCMP.0,
        aggp = I_AGGP.0,
        pmeta = I_PMETA.0,
        sget = I_SGET.0,
    );
    // *_3 (30) + *_8 (40) + offset_from (2) + guaranteed_cmp(p0, p2) (0) + meta (4) = 76.
    tw = tw
        .with_intrinsic(I_OFF, Intrinsic::Offset)
        .with_intrinsic(I_OF, Intrinsic::PtrOffsetFrom)
        .with_intrinsic(I_GCMP, Intrinsic::PtrGuaranteedCmp)
        .with_intrinsic(I_AGGP, Intrinsic::AggregateRawPtr)
        .with_intrinsic(I_PMETA, Intrinsic::PtrMetadata)
        .with_intrinsic(I_SGET, Intrinsic::SliceGetUnchecked)
        .with(ROOT, &body);
    assert_eq!(u32_value(&tw), 76);
}

#[test]
fn intern_live_root_local_and_refuse_dangling_local() {
    let (mut tw, t) = world();
    let ref_u32 = tw.w.ty("&u32").0;

    // Live unscoped root local (e.g., promoted borrow `&42` in a constant initializer) interns as
    // a second allocation pointed to by `_0`.
    let live_body = format!(
        "\
local _0: t{r} return mut
local _1: t{u} temp e0 value
origin 'o0
const c0 = scalar 0x2a: t{u}
bb0:
  _1 = const c0
  _0 = borrow shared _1 L0 'o0
  return
",
        r = ref_u32,
        u = t.u32,
    );
    let tw_live = tw.with(ROOT, &live_body);
    let rec = run(&tw_live, Limits::DEFAULT).expect("expected Done for live root local borrow");
    assert_eq!(rec.allocs.len(), 2);
    assert_eq!(rec.allocs[0].relocs.len(), 1);
    assert_eq!(rec.allocs[0].relocs[0].target, DataTarget::Local(1));
    assert_eq!(rec.allocs[1].bytes.as_slice(), &42u32.to_le_bytes());

    // Scoped local whose storage ends before `return` must be refused as `Dangling`.
    let (mut tw2, t2) = world();
    let ref_u32_2 = tw2.w.ty("&u32").0;
    let dead_body = format!(
        "\
local _0: t{r} return mut
local _1: t{u} temp e0 value
origin 'o0
const c0 = scalar 0x2a: t{u}
bb0:
  live _1
  _1 = const c0
  _0 = borrow shared _1 L0 'o0
  dead _1
  return
",
        r = ref_u32_2,
        u = t2.u32,
    );
    tw2 = tw2.with(ROOT, &dead_body);
    let e = refused(&tw2, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Dangling)), "{e:?}");
}

/// Static interior mutability (`written_in_place` / `DataAlloc::mutable`):
/// matches `blaze-consteval` on `UnsafeCell` structs, tuples, arrays, enums, `&AtomicU8`,
/// `&mut`/`*mut` pointees, plain values, and constants (always read-only), plus the causal
/// control with `UnsafeCell` unmarked.
#[test]
fn static_written_in_place_matches_interior_mutability() {
    let check_case = |cells: bool| {
        let mut w = World::new();
        let cell = w.strukt("UnsafeCell", &[("value", "u8")]);
        if cells {
            w.unsafe_cell_is(cell);
        }
        let atomic = w.strukt("AtomicU8", &[("v", "UnsafeCell")]);
        let slot = w.enum_("Slot", None, &[("Empty", &[][..], None), ("Full", &["UnsafeCell"][..], None)]);
        w.stat("S_ATOMIC", "AtomicU8", "AtomicU8 { v: UnsafeCell { value: 1 } }");
        w.stat("S_TUPLE", "(u8, AtomicU8)", "(2, AtomicU8 { v: UnsafeCell { value: 3 } })");
        w.stat("S_ARR", "[AtomicU8; 2]", "[AtomicU8 { v: UnsafeCell { value: 4 } }, AtomicU8 { v: UnsafeCell { value: 5 } }]");
        w.stat("S_ENUM", "Slot", "Slot::Full(UnsafeCell { value: 6 })");
        w.stat("S_REF_ATOMIC", "&AtomicU8", "&AtomicU8 { v: UnsafeCell { value: 7 } }");
        w.stat("S_MUT_PTR", "*mut u8", "0 as *mut u8");
        w.stat("S_PLAIN", "u32", "9");
        w.konst("K_ATOMIC", "AtomicU8", "AtomicU8 { v: UnsafeCell { value: 1 } }");
        w.build();

        let u8_ty = w.ty("u8").0;
        let u32_ty = w.ty("u32").0;
        let cell_ty = w.ty("UnsafeCell").0;
        let atomic_ty = w.ty("AtomicU8").0;
        let tup_ty = w.ty("(u8, AtomicU8)").0;
        let arr_ty = w.ty("[AtomicU8; 2]").0;
        let slot_ty = w.ty("Slot").0;
        let ref_atomic_ty = w.ty("&AtomicU8").0;
        let mut_ptr_u8 = w.ty("*mut u8").0;

        let s_atomic = w.key("S_ATOMIC");
        let s_tuple = w.key("S_TUPLE");
        let s_arr = w.key("S_ARR");
        let s_enum = w.key("S_ENUM");
        let s_ref_atomic = w.key("S_REF_ATOMIC");
        let s_mut_ptr = w.key("S_MUT_PTR");
        let s_plain = w.key("S_PLAIN");
        let k_atomic = w.key("K_ATOMIC");

        let mut tw = Tw {
            w,
            bodies: StdVec::new(),
            promoted_bodies: StdVec::new(),
            promoted_records: StdVec::new(),
            anon_bodies: StdVec::new(),
            anon_records: StdVec::new(),
            extra_records: StdVec::new(),
            mut_statics: StdVec::new(),
            closures: StdVec::new(),
            custom_adts: StdVec::new(),
            extra_intrinsics: StdVec::new(),
            drop_adts: StdVec::new(),
            fn_once_item: None,
            type_names_on: false,
        };
        let _ = (atomic, slot);
        let mk_atomic = format!(
            "\
local _0: t{atomic_ty} return mut
local _1: t{cell_ty} temp e0 value
const c0 = scalar 0x1: t{u8_ty}
bb0:
  _1 = adt t{cell_ty}#0(const c0)
  _0 = adt t{atomic_ty}#0(move _1)
  return
",
        );
        let mk_tuple = format!(
            "\
local _0: t{tup_ty} return mut
local _1: t{cell_ty} temp e0 value
local _2: t{atomic_ty} temp e1 value
const c0 = scalar 0x2: t{u8_ty}
const c1 = scalar 0x3: t{u8_ty}
bb0:
  _1 = adt t{cell_ty}#0(const c1)
  _2 = adt t{atomic_ty}#0(move _1)
  _0 = tuple t{tup_ty}(const c0, move _2)
  return
",
        );
        let mk_arr = format!(
            "\
local _0: t{arr_ty} return mut
local _1: t{cell_ty} temp e0 value
local _2: t{atomic_ty} temp e1 value
local _3: t{cell_ty} temp e2 value
local _4: t{atomic_ty} temp e3 value
const c0 = scalar 0x4: t{u8_ty}
const c1 = scalar 0x5: t{u8_ty}
bb0:
  _1 = adt t{cell_ty}#0(const c0)
  _2 = adt t{atomic_ty}#0(move _1)
  _3 = adt t{cell_ty}#0(const c1)
  _4 = adt t{atomic_ty}#0(move _3)
  _0 = array t{arr_ty}(move _2, move _4)
  return
",
        );
        let mk_enum = format!(
            "\
local _0: t{slot_ty} return mut
local _1: t{cell_ty} temp e0 value
const c0 = scalar 0x6: t{u8_ty}
bb0:
  _1 = adt t{cell_ty}#0(const c0)
  _0 = adt t{slot_ty}#1(move _1)
  return
",
        );
        let mk_ref_atomic = format!(
            "\
local _0: t{ref_atomic_ty} return mut
local _1: t{cell_ty} temp e0 value
local _2: t{atomic_ty} temp e1 value
origin 'o0
const c0 = scalar 0x7: t{u8_ty}
bb0:
  _1 = adt t{cell_ty}#0(const c0)
  _2 = adt t{atomic_ty}#0(move _1)
  _0 = borrow shared _2 L0 'o0
  return
",
        );
        let mk_mut_ptr = format!(
            "\
local _0: t{mut_ptr_u8} return mut
local _1: t{u8_ty} temp e0 value mut
const c0 = scalar 0x8: t{u8_ty}
bb0:
  _1 = const c0
  _0 = raw mut _1
  return
",
        );
        let mk_plain = format!(
            "\
local _0: t{u32_ty} return mut
const c0 = scalar 0x9: t{u32_ty}
bb0:
  _0 = const c0
  return
",
        );

        tw = tw
            .with(s_atomic, &mk_atomic)
            .with(s_tuple, &mk_tuple)
            .with(s_arr, &mk_arr)
            .with(s_enum, &mk_enum)
            .with(s_ref_atomic, &mk_ref_atomic)
            .with(s_mut_ptr, &mk_mut_ptr)
            .with(s_plain, &mk_plain)
            .with(k_atomic, &mk_atomic);

        let eval_marks = |item: ItemKey| -> OwnVec<bool, Alloc<'static>> {
            let a = heap().alloc();
            let mut layout = Layouts::new_in(a);
            let mut ground = PortableTys::new_in(a);
            let rec = match eval_body(&tw, &mut layout, &mut ground, item, List::EMPTY, Limits::DEFAULT, a).expect("eval") {
                Answer::Done(r) => r,
                Answer::Need(n) => panic!("unexpected need {n:?}"),
            };
            let mut out = OwnVec::with_capacity_in(rec.allocs.len(), a);
            for al in &rec.allocs {
                out.push(al.mutable);
            }
            out
        };

        if cells {
            assert_eq!(eval_marks(s_atomic).as_slice(), &[true]);
            assert_eq!(eval_marks(s_tuple).as_slice(), &[true]);
            assert_eq!(eval_marks(s_arr).as_slice(), &[true]);
            assert_eq!(eval_marks(s_enum).as_slice(), &[true]);
            assert_eq!(eval_marks(s_ref_atomic).as_slice(), &[false, true]);
            assert_eq!(eval_marks(s_mut_ptr).as_slice(), &[false, true]);
            assert_eq!(eval_marks(s_plain).as_slice(), &[false]);
            assert_eq!(eval_marks(k_atomic).as_slice(), &[false]);
        } else {
            assert_eq!(eval_marks(s_atomic).as_slice(), &[false]);
            assert_eq!(eval_marks(s_tuple).as_slice(), &[false]);
            assert_eq!(eval_marks(s_arr).as_slice(), &[false]);
            assert_eq!(eval_marks(s_enum).as_slice(), &[false]);
            assert_eq!(eval_marks(s_ref_atomic).as_slice(), &[false, false]);
            // `*mut u8` still marks its pointee writable even without `UnsafeCell`.
            assert_eq!(eval_marks(s_mut_ptr).as_slice(), &[false, true]);
            assert_eq!(eval_marks(s_plain).as_slice(), &[false]);
            assert_eq!(eval_marks(k_atomic).as_slice(), &[false]);
        }
    };
    check_case(true);
    check_case(false);
}

/// Mirror of `#2466`: promoted field borrows (`&ALIGNED_BYTES.bytes` at offset 0 and an offset-8
/// field inside an align(16) struct) preserve the base struct's alignment (`align == 4` at offset
/// 0, `align == 3` at offset 8) in the interned `DataAlloc`, matching `hashbrown::Group::static_empty()`.
#[test]
fn promoted_field_borrow_preserves_base_struct_alignment() {
    let (mut tw, t) = world();
    let arr16 = tw.w.ty("[u8; 16]");
    let arr8 = tw.w.ty("[u8; 8]");
    let ref_arr16 = tw.w.ty("&[u8; 16]");
    let ref_arr8 = tw.w.ty("&[u8; 8]");
    let u64_t = PTy(t.u64);
    let u8_t = PTy(t.u8);

    // `#[repr(C, align(16))] struct AlignedBytes { bytes: [u8; 16] }` (offset 0 -> align_log2 = 4)
    let k_aligned = ItemKey(0xaa01);
    let aligned_ty = tw.w.fx.t.intern(PNode::Adt(k_aligned, List::EMPTY)).unwrap();
    // `#[repr(C, align(16))] struct AlignedSplit { head: u64, tail: [u8; 8] }` (tail at offset 8 -> align_log2 = 3)
    let k_split = ItemKey(0xaa02);
    let _split_ty = tw.w.fx.t.intern(PNode::Adt(k_split, List::EMPTY)).unwrap();

    tw = tw
        .with_aligned_struct(k_aligned, 4, &[("bytes", arr16)])
        .with_aligned_struct(k_split, 4, &[("head", u64_t), ("tail", arr8)]);

    // Promoted borrow e10: returns `ALIGNED_BYTES.bytes` (`[u8; 16]`, natural align 0 -> must inherit 4).
    let promo_zero = format!(
        "\
local _0: t{a16} return mut
local _1: t{s} temp e0 value
local _2: t{a16} temp e1 value
const c0 = scalar 0x80: t{u8}
const c1 = scalar 0x10: t{us}
bb0:
  _2 = repeat(const c0, c1)
  _1 = adt t{s}#0(move _2)
  _0 = copy _1.0:t{a16}
  return
",
        a16 = arr16.0,
        s = aligned_ty.0,
        u8 = u8_t.0,
        us = t.usize,
    );
    // Root returns `&ALIGNED_BYTES.bytes` via `ConstKind::Promoted(10)`.
    let root_zero = format!(
        "\
local _0: t{r} return mut
const c0 = promoted 10: t{r}
bb0:
  _0 = const c0
  return
",
        r = ref_arr16.0,
    );
    let tw_zero = tw.with(ROOT, &root_zero).with_promoted_body(ROOT, ExprId(10), &promo_zero);
    let rec_zero = run(&tw_zero, Limits::DEFAULT).expect("promoted &ALIGNED_BYTES.bytes evaluates");
    assert_eq!(rec_zero.allocs.len(), 2, "expected pointer root + promoted pointee allocation");
    assert_eq!(rec_zero.allocs[1].bytes.as_slice(), &[0x80u8; 16]);
    assert_eq!(rec_zero.allocs[1].align, 4, "offset-0 field borrow must preserve align(16) (log2 == 4)");

    // Promoted borrow e11: returns `ALIGNED_SPLIT.tail` at offset 8 (`[u8; 8]` -> must inherit min(4, 3) == 3).
    let (mut tw2, t2) = world();
    let arr8_2 = tw2.w.ty("[u8; 8]");
    let ref_arr8_2 = tw2.w.ty("&[u8; 8]");
    let u64_2 = PTy(t2.u64);
    let u8_2 = PTy(t2.u8);
    let split_ty_2 = tw2.w.fx.t.intern(PNode::Adt(k_split, List::EMPTY)).unwrap();
    tw2 = tw2.with_aligned_struct(k_split, 4, &[("head", u64_2), ("tail", arr8_2)]);
    let promo_off8 = format!(
        "\
local _0: t{a8} return mut
local _1: t{s} temp e0 value
local _2: t{a8} temp e1 value
const c0 = scalar 0x55: t{u8}
const c1 = scalar 0x8: t{us}
const c2 = scalar 0x11223344: t{u64}
bb0:
  _2 = repeat(const c0, c1)
  _1 = adt t{s}#0(const c2, move _2)
  _0 = copy _1.1:t{a8}
  return
",
        a8 = arr8_2.0,
        s = split_ty_2.0,
        u8 = u8_2.0,
        us = t2.usize,
        u64 = t2.u64,
    );
    let root_off8 = format!(
        "\
local _0: t{r} return mut
const c0 = promoted 11: t{r}
bb0:
  _0 = const c0
  return
",
        r = ref_arr8_2.0,
    );
    let tw_off8 = tw2.with(ROOT, &root_off8).with_promoted_body(ROOT, ExprId(11), &promo_off8);
    let rec_off8 = run(&tw_off8, Limits::DEFAULT).expect("promoted &ALIGNED_SPLIT.tail evaluates");
    assert_eq!(rec_off8.allocs.len(), 2);
    assert_eq!(rec_off8.allocs[1].bytes.as_slice(), &[0x55u8; 8]);
    assert_eq!(rec_off8.allocs[1].align, 3, "offset-8 field borrow must preserve align(8) (log2 == 3)");
    let _ = ref_arr8;
}
