//! Bodies built by hand as columns, checked by blaze-typeck over a small world, built, verified
//! and run on blaze-lir's evaluator.

use std::alloc::Global;
use std::format;
use std::string::String;

use blaze_lir::eval::{Outcome as Run, Val};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_typeck::toy::ToyCtx;
use blaze_typeck::{
    Adt, AdtKind, Arm, AssocItem, BinOp, BindAnn, BodyPath, BodyView, CtorKind, Def, Edition, Expr, ExprId, Fetch,
    Field, FieldInit, FieldPat, FnSig, Hidden, ImplHeader, IntSuffix, IntTy, ItemKey, LangItem, List, Lit, Name,
    Needs, Outcome, PClause, PNode, PTy, PTys, PatId, PathIdx, PathRes, Pat, PortableTys, Predicates, Range, ScopeId,
    Seal, Segment, Shape, Stmt, StmtId, TypeAttrs, UintTy, Variant, World,
};

use crate::{Adts, Built};

/// A body under construction, with the literal values its form does not hold.
struct BodyBuf {
    exprs: Vec<Expr, Global>,
    expr_ranges: Vec<Range, Global>,
    pats: Vec<Pat, Global>,
    pat_ranges: Vec<Range, Global>,
    stmts: Vec<Stmt, Global>,
    expr_lists: Vec<ExprId, Global>,
    stmt_lists: Vec<StmtId, Global>,
    arms: Vec<Arm, Global>,
    field_inits: Vec<FieldInit, Global>,
    paths: Vec<BodyPath, Global>,
    segments: Vec<Segment, Global>,
    params: Vec<PatId, Global>,
    types: PortableTys<'static>,
    lits: Vec<(ExprId, u128), Global>,
    pat_lits: Vec<(PatId, u128), Global>,
}

impl BodyBuf {
    fn new() -> BodyBuf {
        BodyBuf {
            exprs: Vec::new_in(Global),
            expr_ranges: Vec::new_in(Global),
            pats: Vec::new_in(Global),
            pat_ranges: Vec::new_in(Global),
            stmts: Vec::new_in(Global),
            expr_lists: Vec::new_in(Global),
            stmt_lists: Vec::new_in(Global),
            arms: Vec::new_in(Global),
            field_inits: Vec::new_in(Global),
            paths: Vec::new_in(Global),
            segments: Vec::new_in(Global),
            params: Vec::new_in(Global),
            types: PortableTys::new_in(Alloc::global()),
            lits: Vec::new_in(Global),
            pat_lits: Vec::new_in(Global),
        }
    }

    fn e(&mut self, e: Expr) -> ExprId {
        let at = self.exprs.len() as u32;
        self.exprs.push(e);
        self.expr_ranges.push(Range { start: at, end: at + 1 });
        ExprId(at)
    }

    fn set(&mut self, at: ExprId, e: Expr) {
        self.exprs[at.ix()] = e;
    }

    fn int(&mut self, value: u128, suffix: Option<IntSuffix>) -> ExprId {
        let e = self.e(Expr::Lit(Lit::Int(suffix)));
        self.lits.push((e, value));
        e
    }

    fn bin(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        self.e(Expr::Binary { op, lhs, rhs })
    }

    fn pat(&mut self, p: Pat) -> PatId {
        let at = self.pats.len() as u32;
        self.pats.push(p);
        self.pat_ranges.push(Range { start: at, end: at + 1 });
        PatId(at)
    }

    fn bind(&mut self, name: &str) -> PatId {
        self.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: true }, name: Name::of(name), sub: None })
    }

    fn pat_int(&mut self, value: u128) -> PatId {
        let p = self.pat(Pat::Lit(Lit::Int(None)));
        self.pat_lits.push((p, value));
        p
    }

    fn local(&mut self, p: PatId) -> ExprId {
        self.e(Expr::Local(p))
    }

    fn block(&mut self, stmts: &[Stmt], tail: Option<ExprId>) -> ExprId {
        let start = self.stmt_lists.len() as u32;
        for &s in stmts {
            let id = StmtId(self.stmts.len() as u32);
            self.stmts.push(s);
            self.stmt_lists.push(id);
        }
        let stmts = List { start, len: self.stmt_lists.len() as u32 - start };
        self.e(Expr::Block { stmts, tail, scope: ScopeId(0) })
    }

    fn path(&mut self, name: &str) -> PathIdx {
        let start = self.segments.len() as u32;
        self.segments.push(Segment { name: Name::of(name), generic_args: List::EMPTY, has_args: false });
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: 1 } });
        PathIdx(at)
    }

    fn exprs_list(&mut self, items: &[ExprId]) -> List {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.extend_from_slice(items);
        List { start, len: items.len() as u32 }
    }

    fn view(&self, owner: ItemKey, root: ExprId) -> BodyView<'_> {
        BodyView {
            owner,
            edition: Edition::E2024,
            params: &self.params,
            root,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &self.expr_lists,
            pat_lists: &[],
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &self.field_inits,
            field_pats: &[] as &[FieldPat],
            paths: &self.paths,
            segments: &self.segments,
            types: self.types.view(),
            type_lists: &[],
            opt_types: &[],
        }
    }
}

struct Fun {
    key: ItemKey,
    name: Name,
    inputs: Vec<PTy, Global>,
    output: PTy,
}

struct Struct {
    key: ItemKey,
    variants: Vec<Variant, Global>,
    fields: Vec<Field, Global>,
}

/// Fns and structs by one-segment name; one type table for all their types.
struct TestWorld {
    types: PortableTys<'static>,
    fns: Vec<Fun, Global>,
    structs: Vec<Struct, Global>,
}

impl TestWorld {
    fn new() -> TestWorld {
        TestWorld { types: PortableTys::new_in(Alloc::global()), fns: Vec::new_in(Global), structs: Vec::new_in(Global) }
    }

    fn ty(&mut self, n: PNode) -> PTy {
        self.types.intern(n).expect("a type interns")
    }

    fn func(&mut self, key: u128, name: &str, inputs: &[PNode], output: PNode) {
        let mut ins = Vec::new_in(Global);
        for &n in inputs {
            let t = self.ty(n);
            ins.push(t);
        }
        let output = self.ty(output);
        self.fns.push(Fun { key: ItemKey(key), name: Name::of(name), inputs: ins, output });
    }

    fn strukt(&mut self, key: u128, name: &str, fields: &[(&str, PNode)]) {
        let mut fs = Vec::new_in(Global);
        for &(n, t) in fields {
            let ty = self.ty(t);
            fs.push(Field { name: Name::of(n), ty });
        }
        let mut variants = Vec::new_in(Global);
        variants.push(Variant {
            key: ItemKey(key),
            name: Name::of(name),
            ctor: CtorKind::Braces,
            fields: List { start: 0, len: fs.len() as u32 },
        });
        self.structs.push(Struct { key: ItemKey(key), variants, fields: fs });
    }

    fn sig(&self, key: ItemKey) -> Option<FnSig<'_>> {
        self.fns.iter().find(|f| f.key == key).map(|f| FnSig {
            parent_count: 0,
            own_count: 0,
            self_param: false,
            inputs: &f.inputs,
            output: f.output,
            types: self.types.view(),
        })
    }

    fn adt_of(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.structs.iter().find(|s| s.key == key).map(|s| Adt {
            kind: AdtKind::Struct,
            generics: 0,
            variants: &s.variants,
            fields: &s.fields,
            types: self.types.view(),
        })
    }
}

impl World for TestWorld {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        Ok(self.sig(item))
    }

    fn predicates(&self, _: Seal, _: ItemKey) -> Fetch<Predicates<'_>> {
        Ok(Predicates { clauses: &[] as &[PClause], types: PTys::EMPTY })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        Ok(self.adt_of(item))
    }

    fn impls_of(&self, _: Seal, _: ItemKey, _: Shape) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }

    fn impl_header(&self, _: Seal, _: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        Ok(None)
    }

    fn inherent(&self, _: Seal, _: Shape, _: Name) -> Fetch<&[AssocItem]> {
        Ok(&[])
    }

    fn assoc_item(&self, _: Seal, _: ItemKey, _: Name) -> Fetch<Option<AssocItem>> {
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }

    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<Option<PathRes>> {
        let [n] = segments else { return Ok(None) };
        if let Some(f) = self.fns.iter().find(|f| f.name == *n) {
            return Ok(Some(PathRes { def: Def::Fn(f.key), consumed: 1 }));
        }
        Ok(self.structs.iter().find(|s| s.variants[0].name == *n).map(|s| PathRes { def: Def::Adt(s.key), consumed: 1 }))
    }

    fn lang_item(&self, _: Seal, _: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(None)
    }

    fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> {
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        Ok(None)
    }

    fn attrs(&self, _: Seal, _: ItemKey) -> Fetch<TypeAttrs> {
        Ok(TypeAttrs(0))
    }
}

struct TestAdts<'a> {
    world: &'a TestWorld,
    body: &'a BodyBuf,
}

impl Adts for TestAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.world.adt_of(key)
    }

    fn int_lit(&self, _: ItemKey, expr: ExprId) -> Option<u128> {
        self.body.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn int_pat(&self, _: ItemKey, pat: PatId) -> Option<u128> {
        self.body.pat_lits.iter().find(|(p, _)| *p == pat).map(|&(_, v)| v)
    }
}

const MAIN: ItemKey = ItemKey(1);

/// Checks and builds `body` of `MAIN`.
fn build(world: &TestWorld, body: &BodyBuf, root: ExprId) -> Result<Built<Global>, String> {
    let view = body.view(MAIN, root);
    let mut ty = ToyCtx::new();
    let checked = match blaze_typeck::check(&view, world, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => return Err(format!("the check needed {:?}", n.questions)),
        Err(error) => return Err(format!("the check failed: {error:?}")),
    };
    if !checked.diags.is_empty() {
        return Err(format!("diagnostics: {:?}", checked.diags));
    }
    let sig = world.sig(MAIN).ok_or_else(|| String::from("no signature for main"))?;
    let built = crate::build(&view, &checked, &sig, &TestAdts { world, body }, Global)
        .map_err(|error| format!("the build failed: {error}"))?;
    blaze_lir::verify(built.func.view()).map_err(|error| format!("does not verify: {error}"))?;
    Ok(built)
}

/// Runs a built function, expecting it to return.
fn run(built: &Built<Global>, args: &[Val]) -> Result<Vec<Val, Global>, String> {
    match blaze_lir::eval::run_in(built.func.view(), args, 10_000_000, Global) {
        Ok(Run::Returned(vals)) => Ok(vals),
        Ok(Run::Trapped(t)) => Err(format!("trapped: {t:?}")),
        Ok(Run::Undefined { inst, why }) => Err(format!("undefined at {inst}: {why}")),
        Ok(Run::OutOfFuel) => Err(String::from("out of fuel")),
        Err(error) => Err(format!("the evaluator refused: {error:?}\n{}", blaze_lir::print(built.func.view()))),
    }
}

fn one(built: &Built<Global>, args: &[Val]) -> u128 {
    let vals = run(built, args).unwrap_or_else(|e| panic!("{e}"));
    match vals.as_slice() {
        [Val::Int(v)] => *v,
        other => panic!("returned {other:?}"),
    }
}

const UNIT: PNode = PNode::Tuple(List::EMPTY);
const I32: PNode = PNode::Int(IntTy::I32);

#[test]
fn empty_main_returns_nothing() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let mut b = BodyBuf::new();
    let root = b.block(&[], None);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert!(built.refs.is_empty());
    assert!(run(&built, &[]).unwrap_or_else(|e| panic!("{e}")).is_empty());
}

/// `let mut i = 0; let mut acc = 0; while i < 6 { acc = acc + 7; i += 1; }
/// let r = if acc == 42 { acc } else { 0 - 1 }; r`, the `while` as `loop { if .. else { break } }`.
#[test]
fn arithmetic_if_and_a_loop_make_42() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let i = b.bind("i");
    let acc = b.bind("acc");
    let r = b.bind("r");
    let zero_i = b.int(0, None);
    let zero_acc = b.int(0, None);
    let lp = b.e(Expr::Missing);
    // acc = acc + 7;
    let acc_l = b.local(acc);
    let acc_r = b.local(acc);
    let seven = b.int(7, None);
    let sum = b.bin(BinOp::Add, acc_r, seven);
    let assign = b.e(Expr::Assign { lhs: acc_l, rhs: sum });
    // i += 1;
    let i_l = b.local(i);
    let one_ = b.int(1, None);
    let inc = b.e(Expr::AssignOp { op: BinOp::Add, lhs: i_l, rhs: one_ });
    let then = b.block(&[Stmt::Expr { expr: assign, semi: true }, Stmt::Expr { expr: inc, semi: true }], None);
    let brk = b.e(Expr::Break { value: None, target: lp });
    let else_ = b.block(&[], Some(brk));
    let i_c = b.local(i);
    let six = b.int(6, None);
    let cond = b.bin(BinOp::Lt, i_c, six);
    let iff = b.e(Expr::If { cond, then, else_: Some(else_) });
    let body = b.block(&[], Some(iff));
    b.set(lp, Expr::Loop { body });
    // let r = if acc == 42 { acc } else { 0 - 1 };
    let acc_c = b.local(acc);
    let ft = b.int(42, None);
    let eq = b.bin(BinOp::Eq, acc_c, ft);
    let acc_t = b.local(acc);
    let then2 = b.block(&[], Some(acc_t));
    let z = b.int(0, None);
    let o = b.int(1, None);
    let neg = b.bin(BinOp::Sub, z, o);
    let else2 = b.block(&[], Some(neg));
    let if2 = b.e(Expr::If { cond: eq, then: then2, else_: Some(else2) });
    let r_t = b.local(r);
    let root = b.block(
        &[
            Stmt::Let { pat: i, ty: None, init: Some(zero_i), else_: None },
            Stmt::Let { pat: acc, ty: None, init: Some(zero_acc), else_: None },
            Stmt::Expr { expr: lp, semi: true },
            Stmt::Let { pat: r, ty: None, init: Some(if2), else_: None },
        ],
        Some(r_t),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 42);
}

/// `fn main(x: i32) -> i32 { match x { 1 => 10, 2 => 20, n => n + 100 } }`.
#[test]
fn match_on_an_integer_with_three_arms() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[I32], I32);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let p1 = b.pat_int(1);
    let p2 = b.pat_int(2);
    let n = b.bind("n");
    let ten = b.int(10, None);
    let twenty = b.int(20, None);
    let n_e = b.local(n);
    let hundred = b.int(100, None);
    let plus = b.bin(BinOp::Add, n_e, hundred);
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: p1, guard: None, body: ten });
    b.arms.push(Arm { pat: p2, guard: None, body: twenty });
    b.arms.push(Arm { pat: n, guard: None, body: plus });
    let scrutinee = b.local(x);
    let root = b.e(Expr::Match { scrutinee, arms: List { start, len: 3 } });
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[Val::Int(1)]), 10);
    assert_eq!(one(&built, &[Val::Int(2)]), 20);
    assert_eq!(one(&built, &[Val::Int(5)]), 105);
}

/// `struct P { a: i32, b: u8 }`; `let mut p = P { a: 1, b: 7 }; p.a = 40; p.b = 2; p.a + p.b as i32`.
#[test]
fn a_struct_local_written_and_read() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    w.strukt(9, "P", &[("a", I32), ("b", PNode::Uint(UintTy::U8))]);
    let mut b = BodyBuf::new();
    let p = b.bind("p");
    let path = b.path("P");
    let one_ = b.int(1, None);
    let seven = b.int(7, None);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: one_ });
    b.field_inits.push(FieldInit { name: Name::of("b"), expr: seven });
    let lit = b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: None });
    let set = |b: &mut BodyBuf, field: &str, value: u128| {
        let base = b.local(p);
        let f = b.e(Expr::Field { base, name: Name::of(field) });
        let v = b.int(value, None);
        b.e(Expr::Assign { lhs: f, rhs: v })
    };
    let set_a = set(&mut b, "a", 40);
    let set_b = set(&mut b, "b", 2);
    let base_a = b.local(p);
    let read_a = b.e(Expr::Field { base: base_a, name: Name::of("a") });
    let base_b = b.local(p);
    let read_b = b.e(Expr::Field { base: base_b, name: Name::of("b") });
    let i32_ty = b.types.intern(I32).expect("a type interns");
    let cast = b.e(Expr::Cast { operand: read_b, ty: i32_ty });
    let sum = b.bin(BinOp::Add, read_a, cast);
    let root = b.block(
        &[
            Stmt::Let { pat: p, ty: None, init: Some(lit), else_: None },
            Stmt::Expr { expr: set_a, semi: true },
            Stmt::Expr { expr: set_b, semi: true },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 42);
}

#[test]
fn a_paren_nest_100_000_deep_builds() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let mut e = b.int(42, None);
    for _ in 0..100_000 {
        e = b.e(Expr::Paren(e));
    }
    let built = build(&w, &b, e).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 42);
}

/// `fn f(x: i32) -> i32;` `fn main() -> i32 { f(41) }`: the call names reference 0, which is `f`.
#[test]
fn a_call_names_its_callee() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    w.func(7, "f", &[I32], I32);
    let mut b = BodyBuf::new();
    let path = b.path("f");
    let callee = b.e(Expr::Path(path));
    let arg = b.int(41, None);
    let args = b.exprs_list(&[arg]);
    let root = b.e(Expr::Call { callee, args });
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(built.refs.as_slice(), &[ItemKey(7)]);
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("ref ref0 = func 0"), "{text}");
    assert!(text.lines().any(|l| l.contains("call") && l.contains("ref0")), "{text}");
}

/// One random arithmetic tree over suffixed literals of one type, built in postfix order.
fn arithmetic(tape: &Tape) -> Result<(), Broke> {
    let signed = tape.coin("i32 or u8");
    let (suffix, node, bits) = if signed {
        (IntSuffix::Int(IntTy::I32), I32, 32u32)
    } else {
        (IntSuffix::Uint(UintTy::U8), PNode::Uint(UintTy::U8), 8u32)
    };
    let m = |v: u128| if bits >= 128 { v } else { v & ((1u128 << bits) - 1) };
    let sx = |v: u128| -> i128 {
        let v = m(v);
        if v >> (bits - 1) & 1 == 1 { v as i128 - (1i128 << bits) } else { v as i128 }
    };
    let empty = || Broke::Harness(String::from("the postfix stack ran out"));
    let mut w = TestWorld::new();
    w.func(1, "main", &[], node);
    let mut b = BodyBuf::new();
    // (expression, value, the literal it is if it is one)
    let mut stack: Vec<(ExprId, u128, Option<u128>), Global> = Vec::new_in(Global);
    let steps = tape.range(1..40, "steps");
    for _ in 0..steps {
        if stack.len() < 2 || tape.coin("literal") {
            let top = if signed { 1u128 << 31 } else { 1u128 << 8 };
            let v = u128::from(tape.below(top as u64, "value"));
            let e = b.int(v, Some(suffix));
            stack.push((e, v, Some(v)));
            continue;
        }
        let (re, rv, rlit) = stack.pop().ok_or_else(empty)?;
        let (le, lv, _) = stack.pop().ok_or_else(empty)?;
        let ops = [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::BitAnd, BinOp::BitOr, BinOp::BitXor, BinOp::Div, BinOp::Rem, BinOp::Shl, BinOp::Shr];
        let mut op = ops[tape.index(ops.len(), "op")];
        let allowed = match op {
            BinOp::Div | BinOp::Rem => rlit.is_some_and(|v| v != 0),
            BinOp::Shl | BinOp::Shr => rlit.is_some_and(|v| v < u128::from(bits)),
            _ => true,
        };
        if !allowed {
            op = BinOp::Add;
        }
        let v = match op {
            BinOp::Add => m(lv.wrapping_add(rv)),
            BinOp::Sub => m(lv.wrapping_sub(rv)),
            BinOp::Mul => m(lv.wrapping_mul(rv)),
            BinOp::BitAnd => lv & rv,
            BinOp::BitOr => lv | rv,
            BinOp::BitXor => lv ^ rv,
            BinOp::Div if signed => m(sx(lv).wrapping_div(sx(rv)) as u128),
            BinOp::Div => lv / rv,
            BinOp::Rem if signed => m(sx(lv).wrapping_rem(sx(rv)) as u128),
            BinOp::Rem => lv % rv,
            BinOp::Shl => m(lv << rv),
            BinOp::Shr if signed => m((sx(lv) >> rv) as u128),
            _ => lv >> rv,
        };
        let e = b.bin(op, le, re);
        stack.push((e, v, None));
    }
    while stack.len() > 1 {
        let (re, rv, _) = stack.pop().ok_or_else(empty)?;
        let (le, lv, _) = stack.pop().ok_or_else(empty)?;
        let e = b.bin(BinOp::Add, le, re);
        stack.push((e, m(lv.wrapping_add(rv)), None));
    }
    let (root, want, _) = stack.pop().ok_or_else(empty)?;
    let built = build(&w, &b, root).map_err(Broke::Oracle)?;
    let got = run(&built, &[]).map_err(Broke::Oracle)?;
    match got.as_slice() {
        [Val::Int(v)] if *v == want => Ok(()),
        other => Err(Broke::Oracle(format!("returned {other:?}, Rust's wrapping value is {want}"))),
    }
}

#[test]
fn arithmetic_equals_rust_wrapping_semantics() {
    blaze_sim::check(Config::cases(300), arithmetic);
}
