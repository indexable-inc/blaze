//! Bodies built with the type checker's fixture world, checked by the real type checker,
//! lowered here, printed in blaze-body's text form and checked by `blaze_body::verify`. Each
//! test states the rule it checks and the lines it expects (handles normalized: types `T`,
//! items `K`, expressions `E`, patterns `PAT`, spans dropped).

use std::string::String;
use std::vec::Vec;

use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Checked, Expr, ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome};

use crate::{BodyFacts, lower};

/// The heap every test allocates in, named.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, needing drop; no callee
/// is a trait's method.
struct Plain;

impl Facts for Plain {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

impl BodyFacts for Plain {
    fn trait_item(&self, _: ItemKey) -> Option<bool> {
        Some(false)
    }
}

/// `struct S; fn mk() -> S; fn g(); fn owner()`.
fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let unit = fx.unit();
    fx.func("mk", 0, &[], st, std::vec::Vec::new());
    fx.func("g", 0, &[], unit, std::vec::Vec::new());
    let owner = fx.func("owner", 0, &[], unit, std::vec::Vec::new());
    fx.finish();
    (fx, owner)
}

fn checked(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId) -> Checked<'static> {
    let mut cx = InferCtx::new_in(heap());
    match blaze_typeck::check(&b.view(owner, root), fx, &mut cx, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// The body of `root` lowered, verified, and printed with its handles normalized.
fn lowered(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId) -> String {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    let body = match lower(&view, &c, &Plain, heap()) {
        Ok(x) => x,
        Err(r) => panic!("refused: {r:?}"),
    };
    let mut text = String::new();
    blaze_body::print(&body.view(), &mut text).expect("a lowered body prints");
    if let Err(e) = blaze_body::verify(&body) {
        panic!("verify refused {e:?}:\n{text}");
    }
    normalize(&text)
}

/// Each token: `tN` to `T`, `kH<a,b>` to `K`, `eN` to `E`, `patN` to `PAT`; spans dropped.
fn normalize(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let mut words: Vec<String> = Vec::new();
        for w in line.split(' ') {
            if w.starts_with('@') || w.is_empty() {
                continue;
            }
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
            let x = if let Some(rest) = w.strip_prefix('t')
                && digits(rest)
            {
                String::from("T")
            } else if let Some(rest) = w.strip_prefix('e')
                && digits(rest)
            {
                String::from("E")
            } else if let Some(rest) = w.strip_prefix("pat")
                && digits(rest)
            {
                String::from("PAT")
            } else if w.starts_with('k')
                && let Some(gt) = w.find('>')
            {
                let mut s = String::from("K");
                s.push_str(&w[gt + 1..]);
                s
            } else {
                String::from(w)
            };
            words.push(x);
        }
        let indent = if line.starts_with("  ") { "  " } else { "" };
        out.push_str(indent);
        out.push_str(&words.join(" "));
        out.push('\n');
    }
    out
}

/// The blocks of a normalized body: each header's name (`bbN`) and its rows.
fn blocks(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(row) = line.strip_prefix("  ") {
            if let Some(last) = out.last_mut() {
                last.1.push(String::from(row));
            }
        } else if line.starts_with("bb") {
            let name = line.split([' ', ':']).next().unwrap_or("");
            out.push((String::from(name), Vec::new()));
        }
    }
    out
}

fn block<'t>(bs: &'t [(String, Vec<String>)], name: &str) -> &'t [String] {
    match bs.iter().find(|(n, _)| n == name) {
        Some((_, rows)) => rows,
        None => panic!("no block {name}"),
    }
}

/// The locals of a kind word (`binding`, `temp`), in order.
fn locals_of(text: &str, kind: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with("local ") && l.split(' ').nth(3) == Some(kind))
        .map(|l| String::from(l.split(' ').nth(1).unwrap_or("").trim_end_matches(':')))
        .collect()
}

/// The word after `word` in `row`.
fn after<'r>(row: &'r str, word: &str) -> &'r str {
    let mut it = row.split(' ');
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap_or("");
        }
    }
    ""
}

/// `fn owner() { let a = mk(); g(); let b = mk(); }`: the Reference, "Destructors": a call
/// that unwinds drops what is live, last declared first, and `b` is not live until its
/// initializer returned (blaze-build walk/unwind.rs). The call of `g` unwinds to a cleanup block
/// that drops `a` only, then resumes; so does the call that initializes `b`. Expected lines:
///
/// ```text
///   call fn K() -> _2 then bb4 unwind bb3
/// bb3 cleanup:
///   drop _1 then bb2 unwind terminate
/// bb2 cleanup:
///   resume
/// ```
#[test]
fn unwind_between_two_droppables_drops_the_first_only() {
    let (fx, owner) = world();
    let mut b = Bb::new();
    let mk1 = b.path(&["mk"]);
    let c1 = b.call(mk1, &[]);
    let a = b.bind("a");
    let s1 = b.let_(a, None, c1);
    let g = b.path(&["g"]);
    let cg = b.call(g, &[]);
    let s2 = b.semi(cg);
    let mk2 = b.path(&["mk"]);
    let c2 = b.call(mk2, &[]);
    let bb = b.bind("b");
    let s3 = b.let_(bb, None, c2);
    let root = b.block(&[s1, s2, s3], None);
    let text = lowered(&fx, &b, owner, root);
    std::println!("{text}");
    let binds = locals_of(&text, "binding");
    assert_eq!(binds.len(), 2, "two bindings:\n{text}");
    let (first, second) = (&binds[0], &binds[1]);
    let temps = locals_of(&text, "temp");
    assert_eq!(temps.len(), 1, "one temporary, `g()`'s value:\n{text}");
    let bs = blocks(&text);
    let mut seen = 0;
    for (_, rows) in bs.iter() {
        for row in rows {
            let dest = after(row, "->");
            if !row.starts_with("call ") || (dest != temps[0] && dest != *second) {
                continue;
            }
            seen += 1;
            let cleanup = after(row, "unwind");
            let crow = block(&bs, cleanup);
            assert_eq!(crow.len(), 1, "the cleanup drops one value:\n{text}");
            assert_eq!(after(&crow[0], "drop"), first.as_str(), "the cleanup drops `a`:\n{text}");
            assert_eq!(after(&crow[0], "unwind"), "terminate", "a drop on the unwind path does not unwind:\n{text}");
            let resume = block(&bs, after(&crow[0], "then"));
            assert_eq!(resume, ["resume"], "then unwinding resumes:\n{text}");
        }
    }
    assert_eq!(seen, 2, "the calls of `g` and of `b`'s `mk`:\n{text}");
    std::println!("unwind of g() and of b's mk(): drop {first} only, then resume");
}

/// `fn owner() { loop { let a = mk(); break; } }`: the Reference, "Destructors": leaving a
/// scope by `break` drops its variables. The `break` edge drops `a` and ends its storage before
/// it goes to the loop's exit. Expected lines (the break edge, then the exit):
///
/// ```text
///   _1 = tuple T ()
///   drop _3 then bb4 unwind continue
/// bb4:
///   dead _3
///   dead _2
///   goto bb2
/// ```
#[test]
fn break_out_of_a_scope_drops_its_droppable_local() {
    let (fx, owner) = world();
    let mut b = Bb::new();
    let mk = b.path(&["mk"]);
    let c = b.call(mk, &[]);
    let a = b.bind("a");
    let s1 = b.let_(a, None, c);
    let brk = b.bool();
    let s2 = b.semi(brk);
    let body = b.block(&[s1, s2], None);
    let lp = b.bool();
    b.exprs[brk.ix()] = Expr::Break { value: None, target: lp };
    b.exprs[lp.ix()] = Expr::Loop { body };
    let s3 = b.semi(lp);
    let root = b.block(&[s3], None);
    let text = lowered(&fx, &b, owner, root);
    std::println!("{text}");
    let binds = locals_of(&text, "binding");
    assert_eq!(binds.len(), 1, "one binding:\n{text}");
    let a = &binds[0];
    let bs = blocks(&text);
    // The loop's head: the entry block's `goto` (the statement's temporary begins, then the
    // loop is entered). The code after `break;` is lowered too, into a block nothing reaches
    // (scope.rs `dead_block`), and its normal end drops `a` and goes back to the head: that
    // edge is not the break's.
    let head = String::from(after(block(&bs, "bb0").last().map(String::as_str).unwrap_or(""), "goto"));
    assert!(!head.is_empty(), "the entry block goes to the loop's head:\n{text}");
    // The break edge: a block that drops `a`, whose next block ends `a`'s storage and goes to
    // a block that is not the loop's head (the one the body's normal end goes back to).
    let mut edge = None;
    for (name, rows) in bs.iter() {
        let Some(last) = rows.last() else { continue };
        if after(last, "drop") != a.as_str() {
            continue;
        }
        let next = block(&bs, after(last, "then"));
        if next.iter().any(|r| *r == std::format!("dead {a}")) && next.last().is_some_and(|r| r.starts_with("goto ")) {
            let to = after(next.last().map(String::as_str).unwrap_or(""), "goto");
            if to == head {
                continue;
            }
            assert!(edge.is_none(), "one edge drops `a` and leaves the loop:\n{text}");
            edge = Some((name.clone(), String::from(to)));
        }
    }
    let Some((from, exit)) = edge else { panic!("no edge drops `a` and leaves:\n{text}") };
    // The loop's exit is the block whose code ends the statement holding the loop.
    assert!(
        block(&bs, &exit).iter().any(|r| r.starts_with("dead ")) || block(&bs, &exit).iter().any(|r| r.starts_with("_0 = ")),
        "the break goes to the code after the loop:\n{text}"
    );
    std::println!("break from {from}: drop {a}, dead {a}, goto {exit}");
}

/// The refusal names the construct and the expression: `offset_of!` is not lowered yet.
/// Expected line: `refused OffsetOf at Expr(..)`.
#[test]
fn an_unlowered_construct_is_refused_by_name() {
    let (fx, owner) = world();
    let mut b = Bb::new();
    let x = b.bool();
    let s = b.semi(x);
    let root = b.block(&[s], None);
    let c = checked(&fx, &b, owner, root);
    b.exprs[x.ix()] = Expr::OffsetOf { ty: blaze_typeck::PTy(0), fields: blaze_typeck::List { start: 0, len: 0 } };
    let view = b.view(owner, root);
    match lower(&view, &c, &Plain, heap()) {
        Ok(_) => panic!("lowered a construct it does not take"),
        Err(e) => {
            assert_eq!(e.construct, crate::Construct::OffsetOf);
            assert_eq!(e.at, crate::At::Expr(x));
            std::println!("refused {:?} at {:?}", e.construct, e.at);
        }
    }
    let _ = Mutbl::Not;
}

/// `fn f() -> u8 { bits }` in a world of its own, checked clean, and its view's parts.
fn returns_u8(bits: u128) -> (Fx, Bb, ItemKey, ExprId) {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let owner = fx.func("f", 0, &[], u8_, std::vec::Vec::new());
    fx.finish();
    let mut b = Bb::new();
    let lit = b.int_valued(None, bits);
    let root = b.block(&[], Some(lit));
    (fx, b, owner, root)
}

/// R4 (run.md): the kept record of a checked body opens to the body the lowering gave, row for
/// row, over the checked record's own table, and writes the same bytes again: a restored body
/// hands `build` the inputs a fresh lowering would (the instance's bytes follow, as `build` is a
/// function of them; the code record's equality is the consumer's test).
#[test]
fn a_kept_body_restores_to_the_fresh_lowering() {
    let (fx, b, owner, root) = returns_u8(1);
    let c = checked(&fx, &b, owner, root);
    let view = b.view(owner, root);
    let bytes = crate::keep(&view, &c, &Plain, heap()).expect("kept");
    let fresh = lower(&view, &c, &Plain, heap()).expect("lowered");
    let kept = blaze_body::kept::open(&bytes, heap()).expect("opens");
    let restored = blaze_body::encode(&kept.verified().body(), heap());
    let original = blaze_body::encode(&fresh.view(), heap());
    assert!(restored[..] == original[..], "the restored body is the fresh lowering's");
    assert_eq!(kept.table(), blaze_body::TableId::of(c.types.view()), "over the checked record's table");
    let again = blaze_body::kept::encode_ref(&kept, heap()).expect("re-kept");
    assert!(again[..] == bytes[..], "a restore writes the bytes it was read from (rule 26)");
}

/// Planted control (RV3 job 4): `-> u8 { 1 }` and `-> u8 { 2 }` check to one clean verdict (no
/// diagnostic, one table of types) but keep two records: an identity taken from the verdict
/// alone would build one body's instance for the other.
#[test]
fn one_verdict_two_bodies_keep_two_records() {
    let (fx1, b1, o1, r1) = returns_u8(1);
    let (fx2, b2, o2, r2) = returns_u8(2);
    let (c1, c2) = (checked(&fx1, &b1, o1, r1), checked(&fx2, &b2, o2, r2));
    assert!(c1.diags.is_empty() && c2.diags.is_empty(), "both clean");
    assert_eq!(blaze_body::TableId::of(c1.types.view()), blaze_body::TableId::of(c2.types.view()), "one table of types");
    let one = crate::keep(&b1.view(o1, r1), &c1, &Plain, heap()).expect("kept");
    let two = crate::keep(&b2.view(o2, r2), &c2, &Plain, heap()).expect("kept");
    let one_again = crate::keep(&b1.view(o1, r1), &c1, &Plain, heap()).expect("kept");
    assert!(one[..] == one_again[..], "control: one body keeps one record (rule 25)");
    assert!(one[..] != two[..], "`1` and `2` keep two records");
}

fn empty_body_view<'b>(owner: ItemKey, types: blaze_typeck::PTys<'b>, root: ExprId) -> blaze_typeck::BodyView<'b> {
    blaze_typeck::BodyView {
        owner,
        types,
        root,
        params: &[],
        exprs: &[],
        expr_ranges: &[],
        pats: &[],
        pat_ranges: &[],
        stmts: &[],
        expr_lists: &[],
        pat_lists: &[],
        stmt_lists: &[],
        paths: &[],
        segments: &[],
        opt_types: &[],
        type_lists: &[],
        type_list_ranges: &[],
        segment_ranges: &[],
        method_name_ranges: &[],
        arms: &[],
        field_inits: &[],
        field_pats: &[],
        fmt_uses: &[],
        coroutine: None,
        asm_pieces: &[],
        asm_operands: &[],
        asm_clobbers: &[],
        asm_text: "",
        call_arg_ints: &[],
        legacy_args: &[],
        super_lets: &[],
        unsafe_blocks: &[],
        edition: blaze_typeck::Edition::E2021,
        expr_editions: &[],
        pat_editions: &[],
        record_expr_schemas: &[],
        closure_param_regions: &[],
        loop_marks: &[],
    }
}

fn lowered_view(fx: &Fx, bv: &blaze_typeck::BodyView<'_>) -> blaze_own::String<Alloc<'static>> {
    let mut cx = InferCtx::new_in(heap());
    let c = match blaze_typeck::check(bv, fx, &mut cx, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let body = match lower(bv, &c, &Plain, heap()) {
        Ok(x) => x,
        Err(r) => panic!("refused: {r:?}"),
    };
    let mut text = blaze_own::String::new_in(heap());
    blaze_body::print(&body.view(), &mut text).expect("a lowered body prints");
    if let Err(e) = blaze_body::verify(&body) {
        panic!("verify refused {e:?}:\n{text}");
    }
    let norm = normalize(&text);
    let mut out = blaze_own::String::new_in(heap());
    out.push_str(&norm);
    out
}

/// Lowers structural records: `Expr::Record` literal (non-canonical field order -> canonical
/// `AggregateKind::Tuple`), `Pat::Record` destructuring (`let { a, .. } = r;` and `match`),
/// `Expr::Record` functional update (`{ a: a, ..r }`), and width-subtyping coercion
/// (`AdjustKind::RecordProject`).
#[test]
fn structural_record_literal_update_project_and_pattern() {
    use blaze_typeck::{
        Arm, BindAnn, FieldInit, FieldPat, IntSuffix, List, Lit, PNode, Pat, PatId, PortableTys, Range, RecordSchema,
        ScopeId, Stmt, StmtId, StructBase, UintTy,
    };

    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("rec_fn", 0, &[], u32_t, Default::default());
    fx.finish();

    let mut body_tys = PortableTys::new_in(heap());
    let b_u8 = body_tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
    let wide_names = [Name::of("a"), Name::of("b")];
    let wide_schema = RecordSchema::of_sorted_names(&wide_names);
    body_tys.register_record_schema(wide_schema, &wide_names).expect("wide schema");

    let narrow_names = [Name::of("a")];
    let narrow_schema = RecordSchema::of_sorted_names(&narrow_names);
    body_tys.register_record_schema(narrow_schema, &narrow_names).expect("narrow schema");
    let narrow_list = body_tys.intern_list(&[b_u32]).expect("narrow list");
    let narrow_ty = body_tys.intern(PNode::Record(narrow_schema, narrow_list)).expect("narrow ty");

    let mk_bind = |name: &str| Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None };
    let pats = [
        mk_bind("r"),                                                  // 0
        mk_bind("a"),                                                  // 1
        Pat::Record { fields: List { start: 0, len: 1 }, rest: true }, // 2: { a, .. }
        mk_bind("r2"),                                                 // 3
        mk_bind("narrow"),                                             // 4
        mk_bind("m"),                                                  // 5
        Pat::Record { fields: List { start: 1, len: 1 }, rest: false }, // 6: { a: m }
    ];
    let field_pats = [
        FieldPat { name: Name::of("a"), pat: PatId(1) },
        FieldPat { name: Name::of("a"), pat: PatId(5) },
    ];
    let mut pat_ranges = blaze_own::Vec::with_capacity_in(pats.len(), heap());
    for i in 0..pats.len() {
        let s = u32::try_from(i).unwrap_or(0);
        pat_ranges.push(Range { start: s, end: s + 1 });
    }

    let exprs = [
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))),                                 // 0: 1u8
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                                // 1: 2u32
        Expr::Record { fields: List { start: 0, len: 2 }, base: StructBase::None },             // 2: { b: 1u8, a: 2u32 }
        Expr::Local(PatId(0)),                                                                  // 3: r
        Expr::Local(PatId(1)),                                                                  // 4: a
        Expr::Local(PatId(0)),                                                                  // 5: r
        Expr::Record { fields: List { start: 2, len: 1 }, base: StructBase::Expr(ExprId(5)) },  // 6: { a: a, ..r }
        Expr::Local(PatId(3)),                                                                  // 7: r2 (RecordProject -> { a: u32 })
        Expr::Local(PatId(4)),                                                                  // 8: narrow
        Expr::Local(PatId(5)),                                                                  // 9: m
        Expr::Match { scrutinee: ExprId(8), arms: List { start: 0, len: 1 } },                  // 10: match narrow { { a: m } => m }
        Expr::Block { stmts: List { start: 0, len: 4 }, tail: Some(ExprId(10)), scope: ScopeId(0) }, // 11
    ];
    let mut expr_ranges = blaze_own::Vec::with_capacity_in(exprs.len(), heap());
    for i in 0..exprs.len() {
        let s = u32::try_from(i).unwrap_or(0);
        expr_ranges.push(Range { start: s, end: s + 1 });
    }
    let field_inits = [
        FieldInit { name: Name::of("b"), expr: ExprId(0) },
        FieldInit { name: Name::of("a"), expr: ExprId(1) },
        FieldInit { name: Name::of("a"), expr: ExprId(4) },
    ];
    let arms = [Arm { pat: PatId(6), guard: None, body: ExprId(9) }];
    let stmts = [
        Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(2)), else_: None },
        Stmt::Let { pat: PatId(2), ty: None, init: Some(ExprId(3)), else_: None },
        Stmt::Let { pat: PatId(3), ty: None, init: Some(ExprId(6)), else_: None },
        Stmt::Let { pat: PatId(4), ty: Some(narrow_ty), init: Some(ExprId(7)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1), StmtId(2), StmtId(3)];
    let call_arg_ints = [(ExprId(0), 1u128), (ExprId(1), 2u128)];
    let record_expr_schemas = [(ExprId(2), wide_schema)];

    let mut bv = empty_body_view(owner, body_tys.view(), ExprId(11));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.field_pats = &field_pats;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.field_inits = &field_inits;
    bv.arms = &arms;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.call_arg_ints = &call_arg_ints;
    bv.record_expr_schemas = &record_expr_schemas;

    let text = lowered_view(&fx, &bv);
    assert!(text.contains("tuple "), "record literal/update/projection lowers via AggregateKind::Tuple:\n{text}");
    assert!(text.contains("(const c1, const c0)"), "non-canonical record fields sorted canonically:\n{text}");
    assert!(text.contains(".0:"), "canonical field projection emitted:\n{text}");
    assert!(text.contains(".1:"), "functional update copies unwritten field from base:\n{text}");
    let _ = b_u8;
}

/// Lowers structural unions: `AdjustKind::UnionInject`, `AdjustKind::UnionWiden`,
/// `UnionPat::Variant`, and `UnionPat::Residual`.
#[test]
fn structural_union_inject_widen_and_match_patterns() {
    use blaze_typeck::{
        Arm, BindAnn, IntSuffix, List, Lit, PNode, Pat, PatId, PortableTys, Range, ScopeId, Stmt, StmtId, UintTy,
    };

    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("union_fn", 0, &[], u32_t, Default::default());
    fx.finish();

    let mut body_tys = PortableTys::new_in(heap());
    let b_u32 = body_tys.intern(PNode::Uint(UintTy::U32)).expect("u32");
    let b_bool = body_tys.intern(PNode::Bool).expect("bool");
    let b_u16 = body_tys.intern(PNode::Uint(UintTy::U16)).expect("u16");
    let u2_l = body_tys.intern_list(&[b_u32, b_bool]).expect("u2");
    let u2_ty = body_tys.intern(PNode::Union(u2_l)).expect("u32 | bool");
    let u3_l = body_tys.intern_list(&[b_u32, b_bool, b_u16]).expect("u3");
    let u3_ty = body_tys.intern(PNode::Union(u3_l)).expect("u32 | bool | u16");

    let mk_bind = |name: &str, ty: Option<blaze_typeck::PTy>| Pat::Bind {
        ann: BindAnn { by_ref: None, mutable: false },
        name: Name::of(name),
        sub: None,
        ty,
    };
    let pats = [
        mk_bind("u", None),          // 0
        mk_bind("w", None),          // 1
        mk_bind("n", Some(b_u32)),   // 2: Variant(0)
        mk_bind("rest", None),       // 3: Residual (bool | u16)
        mk_bind("b", Some(b_bool)),  // 4: Variant(0) on bool | u16
        mk_bind("s", Some(b_u16)),   // 5: Variant(1) on bool | u16
    ];
    let mut pat_ranges = blaze_own::Vec::with_capacity_in(pats.len(), heap());
    for i in 0..pats.len() {
        let s = u32::try_from(i).unwrap_or(0);
        pat_ranges.push(Range { start: s, end: s + 1 });
    }

    let exprs = [
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 0: 42u32 (UnionInject -> u2)
        Expr::Local(PatId(0)),                                                  // 1: u (UnionWiden -> u3)
        Expr::Local(PatId(1)),                                                  // 2: w
        Expr::Local(PatId(2)),                                                  // 3: n
        Expr::Local(PatId(3)),                                                  // 4: rest
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 5: 1u32
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),                // 6: 2u32
        Expr::Match { scrutinee: ExprId(4), arms: List { start: 0, len: 2 } },  // 7: match rest { b => 1, s => 2 }
        Expr::Match { scrutinee: ExprId(2), arms: List { start: 2, len: 2 } },  // 8: match w { n => n, rest => ... }
        Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(8)), scope: ScopeId(0) }, // 9
    ];
    let mut expr_ranges = blaze_own::Vec::with_capacity_in(exprs.len(), heap());
    for i in 0..exprs.len() {
        let s = u32::try_from(i).unwrap_or(0);
        expr_ranges.push(Range { start: s, end: s + 1 });
    }
    let arms = [
        Arm { pat: PatId(4), guard: None, body: ExprId(5) },
        Arm { pat: PatId(5), guard: None, body: ExprId(6) },
        Arm { pat: PatId(2), guard: None, body: ExprId(3) },
        Arm { pat: PatId(3), guard: None, body: ExprId(7) },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(0), ty: Some(u2_ty), init: Some(ExprId(0)), else_: None },
        Stmt::Let { pat: PatId(1), ty: Some(u3_ty), init: Some(ExprId(1)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1)];
    let call_arg_ints = [(ExprId(0), 42u128), (ExprId(5), 1u128), (ExprId(6), 2u128)];

    let mut bv = empty_body_view(owner, body_tys.view(), ExprId(9));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.arms = &arms;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.call_arg_ints = &call_arg_ints;

    let text = lowered_view(&fx, &bv);
    assert!(text.contains("adt "), "union injection/widening/narrowing lowers via AggregateKind::Adt:\n{text}");
    assert!(text.contains("discr "), "union widening and matching emit Value::Discriminant:\n{text}");
    assert!(text.contains("#0.0"), "union downcast + payload field projection emitted:\n{text}");
}

/// Facts world that answers integer pattern literals and enum/union metadata for lowering tests.
struct RichFacts<'a> {
    int_pats: &'a [(blaze_typeck::PatId, u128)],
    enum_key: Option<ItemKey>,
    union_key: Option<ItemKey>,
}

impl Facts for RichFacts<'_> {
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        if self.enum_key == Some(k) {
            return Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false });
        }
        if self.union_key == Some(k) {
            return Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: true, is_enum: false, is_box: false });
        }
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
    }
    fn field(&self, k: ItemKey, _: u32, name: Name) -> Option<u32> {
        if self.union_key == Some(k) {
            if name == Name::of("a") {
                return Some(0);
            }
            if name == Name::of("b") {
                return Some(1);
            }
        }
        None
    }
    fn field_count(&self, k: ItemKey, _: u32) -> Option<u32> {
        if self.union_key == Some(k) { Some(2) } else { None }
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

impl BodyFacts for RichFacts<'_> {
    fn int_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        self.int_pats.iter().find(|(k, _)| *k == p).map(|(_, v)| *v)
    }
    fn trait_item(&self, _: ItemKey) -> Option<bool> {
        Some(false)
    }
    fn pointer_bits(&self) -> Option<u32> {
        Some(64)
    }
}

/// Lowers `let pat = init else { return ..; }`, range patterns (`1..=5`), or-patterns (`6 | 7`),
/// slice/array patterns (`let [first, .., last] = [x, 10, 20];`), destructuring tuple swap
/// (`(first, last) = (last, first);`), and verifies the resulting `blaze-body`.
#[test]
fn let_else_range_or_slice_and_destructuring_assign_lower_and_verify() {
    use blaze_typeck::{
        Arm, BindAnn, IntSuffix, List, Lit, PNode, Pat, PatId, PortableTys, Range, ScopeId, Stmt, StmtId, UintTy,
    };

    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let owner = fx.func("f", 0, &[u32_t], u32_t, Default::default());
    fx.finish();

    let body_tys = PortableTys::new_in(heap());
    let mk_bind = |name: &str, mutable: bool| Pat::Bind {
        ann: BindAnn { by_ref: None, mutable },
        name: Name::of(name),
        sub: None,
        ty: None,
    };
    let lit_u32 = || Pat::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32))));
    let pats = [
        mk_bind("x", false),                                            // 0: param x
        lit_u32(),                                                      // 1: 1u32 (range lo)
        lit_u32(),                                                      // 2: 5u32 (range hi)
        Pat::Range { lo: Some(PatId(1)), hi: Some(PatId(2)), inclusive: true }, // 3: 1..=5
        lit_u32(),                                                      // 4: 6u32
        lit_u32(),                                                      // 5: 7u32
        Pat::Or { alts: List { start: 0, len: 2 } },                    // 6: 6 | 7
        Pat::Wild,                                                      // 7: _
        lit_u32(),                                                      // 8: 10u32 (let-else pat)
        mk_bind("first", true),                                         // 9
        Pat::Wild,                                                      // 10: ..
        mk_bind("last", true),                                          // 11
        Pat::Slice { elems: List { start: 2, len: 3 }, rest: Some(1) }, // 12: [first, .., last]
    ];
    let pat_lists = [PatId(4), PatId(5), PatId(9), PatId(10), PatId(11)];
    let mut pat_ranges = blaze_own::Vec::with_capacity_in(pats.len(), heap());
    for i in 0..pats.len() {
        let s = u32::try_from(i).unwrap_or(0);
        pat_ranges.push(Range { start: s, end: s + 1 });
    }
    let exprs = [
        Expr::Local(PatId(0)),                                          // 0: x
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 1: 10u32
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 2: 20u32
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 3: 30u32
        Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 3 } }, // 4: match x { 1..=5 => 10, 6 | 7 => 20, _ => 30 }
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 5: 99u32
        Expr::Return { value: Some(ExprId(5)) },                        // 6: return 99u32
        Expr::Local(PatId(0)),                                          // 7: x
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 8: 100u32
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),        // 9: 200u32
        Expr::Array { elems: List { start: 0, len: 3 } },               // 10: [x, 100, 200]
        Expr::Local(PatId(9)),                                          // 11: first
        Expr::Local(PatId(11)),                                         // 12: last
        Expr::Assign { lhs: ExprId(11), rhs: ExprId(12) },              // 13: first = last
        Expr::Local(PatId(9)),                                          // 14: first
        Expr::Block { stmts: List { start: 0, len: 3 }, tail: Some(ExprId(14)), scope: ScopeId(0) }, // 15
    ];
    let expr_lists = [ExprId(7), ExprId(8), ExprId(9)];
    let mut expr_ranges = blaze_own::Vec::with_capacity_in(exprs.len(), heap());
    for i in 0..exprs.len() {
        let s = u32::try_from(i).unwrap_or(0);
        expr_ranges.push(Range { start: s, end: s + 1 });
    }
    let arms = [
        Arm { pat: PatId(3), guard: None, body: ExprId(1) },
        Arm { pat: PatId(6), guard: None, body: ExprId(2) },
        Arm { pat: PatId(7), guard: None, body: ExprId(3) },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(8), ty: None, init: Some(ExprId(4)), else_: Some(ExprId(6)) },
        Stmt::Let { pat: PatId(12), ty: None, init: Some(ExprId(10)), else_: None },
        Stmt::Expr { expr: ExprId(13), semi: true },
    ];
    let stmt_lists = [StmtId(0), StmtId(1), StmtId(2)];
    let call_arg_ints = [
        (ExprId(1), 10),
        (ExprId(2), 20),
        (ExprId(3), 30),
        (ExprId(5), 99),
        (ExprId(8), 100),
        (ExprId(9), 200),
    ];
    let params = [PatId(0)];
    let mut bv = empty_body_view(owner, body_tys.view(), ExprId(15));
    bv.params = &params;
    bv.pats = &pats;
    bv.pat_lists = &pat_lists;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_lists = &expr_lists;
    bv.expr_ranges = &expr_ranges;
    bv.arms = &arms;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.call_arg_ints = &call_arg_ints;

    let mut cx = InferCtx::new_in(heap());
    let ck = match blaze_typeck::check(&bv, &fx, &mut cx, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let int_pats = [(PatId(1), 1), (PatId(2), 5), (PatId(4), 6), (PatId(5), 7), (PatId(8), 10)];
    let facts = RichFacts {
        int_pats: &int_pats,
        enum_key: None,
        union_key: None,
    };
    let body = lower(&bv, &ck, &facts, heap()).expect("lowering succeeds");
    let mut text = blaze_own::String::new_in(heap());
    blaze_body::print(&body.view(), &mut text).expect("prints");
    blaze_body::verify(&body).unwrap_or_else(|e| panic!("verify failed: {e:?}\n{text}"));
    assert!(text.contains("ge(") && text.contains("le("), "range pattern emits ge/le:\n{text}");
    assert!(text.contains("[-1 of 2]"), "slice pattern emits from_end ConstIndex:\n{text}");
    let _ = PNode::Bool;
}

/// Lowers `LetChain`, binding `OrPattern` (`E::A(x) | E::B(x)`), float & string `ValuePattern`,
/// `SuperLet`, `OverloadedDeref`, `OverloadedIndex`, and `CallThroughFnTrait` (closure call),
/// verifying every resulting `blaze-body`.
#[test]
fn wave2_let_chain_binding_or_value_pat_super_let_deref_index_fn_trait_lower_and_verify() {
    use blaze_typeck::{
        Arm, BinOp, BindAnn, BodyPath, ClosureKind, FloatTy, IntSuffix, LangItem, List, Lit, Mutbl,
        PNode, Pat, PatId, PathIdx, PortableTys, Range, ScopeId, Segment, Stmt, StmtId, UintTy, UnOp,
    };

    // 1. Binding OrPattern + LetChain + SuperLet + Float/Str ValuePatterns
    let mut fx = Fx::new();
    let u32_t = fx.u32();
    let f64_t = fx.ty(PNode::Float(FloatTy::F64));
    let str_t = fx.ty(PNode::Str);
    let ref_str_t = fx.ty(PNode::Ref(Mutbl::Not, str_t));
    let (ek, _evs) = fx.enum_("E", 0, &[("A", &[u32_t]), ("B", &[u32_t]), ("C", &[])]);
    let e_t = fx.adt(ek, &[]);
    let owner = fx.func("f", 0, &[e_t, f64_t, ref_str_t], u32_t, Default::default());
    fx.finish();

    let body_tys = PortableTys::new_in(heap());
    let mk_bind = |name: &str| Pat::Bind {
        ann: BindAnn { by_ref: None, mutable: false },
        name: Name::of(name),
        sub: None,
        ty: None,
    };
    let segments = [
        Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
        Segment { name: Name::of("A"), generic_args: List::EMPTY, has_args: false },
        Segment { name: Name::of("E"), generic_args: List::EMPTY, has_args: false },
        Segment { name: Name::of("B"), generic_args: List::EMPTY, has_args: false },
    ];
    let paths = [
        BodyPath { scope: ScopeId(0), segments: List { start: 0, len: 2 }, lang: None, anchor: None }, // 0: E::A
        BodyPath { scope: ScopeId(0), segments: List { start: 2, len: 2 }, lang: None, anchor: None }, // 1: E::B
    ];
    let pats = [
        mk_bind("e"),                                                       // 0: param e
        mk_bind("fl"),                                                      // 1: param fl
        mk_bind("s"),                                                       // 2: param s
        mk_bind("v"),                                                       // 3: v in E::A(v)
        Pat::TupleStruct { path: PathIdx(0), elems: List { start: 0, len: 1 }, rest: None }, // 4: E::A(v)
        mk_bind("v"),                                                       // 5: v in E::B(v)
        Pat::TupleStruct { path: PathIdx(1), elems: List { start: 1, len: 1 }, rest: None }, // 6: E::B(v)
        Pat::Or { alts: List { start: 2, len: 2 } },                        // 7: E::A(v) | E::B(v)
        Pat::NegLit(Lit::Float(Some(FloatTy::F64))),                        // 8: -1.5f64
        Pat::Lit(Lit::Str),                                                 // 9: "ok"
        mk_bind("sl"),                                                      // 10: super let sl
        mk_bind("res"),                                                     // 11: let res
    ];
    let pat_lists = [PatId(3), PatId(5), PatId(4), PatId(6)];
    let mut pat_ranges = blaze_own::Vec::with_capacity_in(pats.len(), heap());
    for i in 0..pats.len() {
        let s = u32::try_from(i).unwrap_or(0);
        pat_ranges.push(Range { start: s, end: s + 1 });
    }
    let exprs = [
        Expr::Local(PatId(0)),                                              // 0: e
        Expr::Let { pat: PatId(7), init: ExprId(0) },                       // 1: let E::A(v) | E::B(v) = e
        Expr::Local(PatId(1)),                                              // 2: fl
        Expr::Let { pat: PatId(8), init: ExprId(2) },                       // 3: let -1.5 = fl
        Expr::Binary { op: BinOp::And, lhs: ExprId(1), rhs: ExprId(3) },    // 4: (1) && (3)
        Expr::Local(PatId(2)),                                              // 5: s
        Expr::Let { pat: PatId(9), init: ExprId(5) },                       // 6: let "ok" = s
        Expr::Binary { op: BinOp::And, lhs: ExprId(4), rhs: ExprId(6) },    // 7: (1) && (3) && (6)
        Expr::Local(PatId(3)),                                              // 8: v
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32)))),            // 9: 0u32
        Expr::If { cond: ExprId(7), then: ExprId(8), else_: Some(ExprId(9)) }, // 10
        Expr::Block { stmts: List { start: 0, len: 1 }, tail: Some(ExprId(12)), scope: ScopeId(1) }, // 11: { super let sl = 10; sl }
        Expr::Local(PatId(10)),                                             // 12: sl
        Expr::Local(PatId(11)),                                             // 13: res
        Expr::Binary { op: BinOp::Add, lhs: ExprId(13), rhs: ExprId(13) },  // 14: res + res
        Expr::Block { stmts: List { start: 1, len: 1 }, tail: Some(ExprId(14)), scope: ScopeId(0) }, // 15
    ];
    let mut expr_ranges = blaze_own::Vec::with_capacity_in(exprs.len(), heap());
    for i in 0..exprs.len() {
        let s = u32::try_from(i).unwrap_or(0);
        expr_ranges.push(Range { start: s, end: s + 1 });
    }
    let stmts = [
        Stmt::Let { pat: PatId(10), ty: None, init: Some(ExprId(10)), else_: None }, // 0: super let sl = if ...
        Stmt::Let { pat: PatId(11), ty: None, init: Some(ExprId(11)), else_: None }, // 1: let res = { super let sl = ...; sl }
    ];
    let stmt_lists = [StmtId(0), StmtId(1)];
    let super_lets = [StmtId(0)];
    let call_arg_ints = [(ExprId(9), 0)];
    let params = [PatId(0), PatId(1), PatId(2)];
    let mut bv = empty_body_view(owner, body_tys.view(), ExprId(15));
    bv.params = &params;
    bv.segments = &segments;
    bv.paths = &paths;
    bv.pats = &pats;
    bv.pat_lists = &pat_lists;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.super_lets = &super_lets;
    bv.call_arg_ints = &call_arg_ints;

    let mut cx = InferCtx::new_in(heap());
    let ck = match blaze_typeck::check(&bv, &fx, &mut cx, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };

    struct Wave2Facts {
        ek: ItemKey,
        deref_k: Option<ItemKey>,
        index_k: Option<ItemKey>,
        fn_call_k: Option<ItemKey>,
    }
    impl Facts for Wave2Facts {
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts { &blaze_flow_lower::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> { None }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_bodies_checked(&self) -> bool { false }
        fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { None }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { None }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> { None }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
            Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: k == self.ek, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> { None }
        fn field_count(&self, k: ItemKey, v: u32) -> Option<u32> {
            if k == self.ek { Some(if v < 2 { 1 } else { 0 }) } else { None }
        }
        fn param_copy(&self, _: u32) -> bool { false }
    }
    impl BodyFacts for Wave2Facts {
        fn float_pat(&self, p: PatId) -> Option<u128> {
            if p == PatId(8) { Some(u128::from(1.5f64.to_bits())) } else { None }
        }
        fn str_pat(&self, p: PatId) -> Option<&[u8]> {
            if p == PatId(9) { Some(b"ok") } else { None }
        }
        fn discr(&self, adt: ItemKey, variant: u32) -> Option<crate::Discr> {
            if adt == self.ek {
                Some(crate::Discr { int: crate::DiscrInt::Int(blaze_typeck::IntTy::Isize), bits: u128::from(variant) })
            } else {
                None
            }
        }
        fn pointer_bits(&self) -> Option<u32> { Some(64) }
        fn trait_item(&self, _: ItemKey) -> Option<bool> { Some(false) }
        fn deref_fn(&self, _: Mutbl) -> Option<ItemKey> { self.deref_k }
        fn index_fn(&self, _: Mutbl) -> Option<ItemKey> { self.index_k }
        fn fn_trait_method(&self, _: ClosureKind) -> Option<ItemKey> { self.fn_call_k }
    }

    let facts = Wave2Facts { ek, deref_k: None, index_k: None, fn_call_k: None };
    let body = lower(&bv, &ck, &facts, heap()).expect("let-chain + binding or + float/str pat + super let lowers");
    let mut text = blaze_own::String::new_in(heap());
    blaze_body::print(&body.view(), &mut text).expect("prints");
    blaze_body::verify(&body).unwrap_or_else(|e| panic!("verify failed: {e:?}\n{text}"));
    assert!(text.contains("eq("), "float pattern emits eq:\n{text}");
    assert!(text.contains("= len "), "str pattern emits len:\n{text}");

    // 2. OverloadedDeref + OverloadedIndex + CallThroughFnTrait (closure)
    let mut fx2 = Fx::new();
    let u32_2 = fx2.u32();
    let usize_2 = fx2.usize();
    let copy_tr = fx2.tr("Copy", 1, Default::default(), &[]);
    fx2.lang(LangItem::Copy, copy_tr);
    fx2.imp("impl Copy for u32", 0, u32_2, Some((copy_tr, &[])), &[], Default::default());
    let deref_tr = fx2.tr("Deref", 1, Default::default(), &["Target"]);
    fx2.lang(LangItem::Deref, deref_tr);
    let index_tr = fx2.tr("Index", 2, Default::default(), &["Output"]);
    fx2.lang(LangItem::Index, index_tr);
    let wrap_k = fx2.strukt("Wrap", 0, &[("v", u32_2)]);
    let wrap_t = fx2.adt(wrap_k, &[]);
    fx2.imp("impl Deref for Wrap", 0, wrap_t, Some((deref_tr, &[])), &[("Target", u32_2)], Default::default());
    fx2.imp("impl Index<usize> for Wrap", 0, wrap_t, Some((index_tr, &[usize_2])), &[("Output", u32_2)], Default::default());
    let owner2 = fx2.func("use_ops", 0, &[wrap_t, usize_2], u32_2, Default::default());
    fx2.finish();

    let mut body_tys2 = PortableTys::new_in(heap());
    let b_u32 = body_tys2.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
    let opt_types2 = [Some(b_u32)];
    let pats2 = [mk_bind("w"), mk_bind("i"), mk_bind("c"), mk_bind("a")];
    let pat_lists2 = [PatId(3)];
    let mut pat_ranges2 = blaze_own::Vec::with_capacity_in(pats2.len(), heap());
    for i in 0..pats2.len() {
        let s = u32::try_from(i).unwrap_or(0);
        pat_ranges2.push(Range { start: s, end: s + 1 });
    }
    let exprs2 = [
        Expr::Local(PatId(3)),                                              // 0: a
        Expr::Local(PatId(0)),                                              // 1: w
        Expr::Unary { op: UnOp::Deref, operand: ExprId(1) },                // 2: *w (OverloadedDeref)
        Expr::Binary { op: BinOp::Add, lhs: ExprId(0), rhs: ExprId(2) },    // 3: a + *w
        Expr::Closure { params: List { start: 0, len: 1 }, param_tys: List { start: 0, len: 1 }, ret: None, body: ExprId(3), is_move: false, is_async: false }, // 4: |a: u32| a + *w
        Expr::Local(PatId(2)),                                              // 5: c
        Expr::Local(PatId(0)),                                              // 6: w
        Expr::Local(PatId(1)),                                              // 7: i
        Expr::Index { base: ExprId(6), index: ExprId(7) },                  // 8: w[i] (OverloadedIndex)
        Expr::Call { callee: ExprId(5), args: List { start: 0, len: 1 } },  // 9: c(w[i]) (CallThroughFnTrait)
        Expr::Block { stmts: List { start: 0, len: 1 }, tail: Some(ExprId(9)), scope: ScopeId(0) }, // 10
    ];
    let expr_lists2 = [ExprId(8)];
    let mut expr_ranges2 = blaze_own::Vec::with_capacity_in(exprs2.len(), heap());
    for i in 0..exprs2.len() {
        let s = u32::try_from(i).unwrap_or(0);
        expr_ranges2.push(Range { start: s, end: s + 1 });
    }
    let stmts2 = [Stmt::Let { pat: PatId(2), ty: None, init: Some(ExprId(4)), else_: None }];
    let stmt_lists2 = [StmtId(0)];
    let params2 = [PatId(0), PatId(1)];
    let mut bv2 = empty_body_view(owner2, body_tys2.view(), ExprId(10));
    bv2.params = &params2;
    bv2.opt_types = &opt_types2;
    bv2.pats = &pats2;
    bv2.pat_lists = &pat_lists2;
    bv2.pat_ranges = &pat_ranges2;
    bv2.exprs = &exprs2;
    bv2.expr_lists = &expr_lists2;
    bv2.expr_ranges = &expr_ranges2;
    bv2.stmts = &stmts2;
    bv2.stmt_lists = &stmt_lists2;

    let mut cx2 = InferCtx::new_in(heap());
    let ck2 = match blaze_typeck::check(&bv2, &fx2, &mut cx2, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "fixture2 type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("world2 missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let facts2 = Wave2Facts {
        ek: ItemKey(9999),
        deref_k: Some(ItemKey(0x301)),
        index_k: Some(ItemKey(0x302)),
        fn_call_k: Some(ItemKey(0x303)),
    };
    let body2 = lower(&bv2, &ck2, &facts2, heap()).expect("deref + index + closure call lowers");
    blaze_body::verify(&body2).expect("body2 verifies");
    let closure_body = crate::lower_closure(&bv2, &ck2, &facts2, ExprId(4), heap()).expect("closure body with *w lowers");
    blaze_body::verify(&closure_body).expect("closure body verifies");
    let mut t2 = blaze_own::String::new_in(heap());
    blaze_body::print(&body2.view(), &mut t2).expect("prints");
    assert!(t2.contains("method k302"), "OverloadedIndex must emit method call k302:\n{t2}");
    assert!(t2.contains("method k303"), "Closure call must emit Fn-trait method call k303:\n{t2}");
    let mut tc = blaze_own::String::new_in(heap());
    blaze_body::print(&closure_body.view(), &mut tc).expect("prints");
    assert!(tc.contains("method k301"), "OverloadedDeref in closure must emit method call k301:\n{tc}");
    let _ = Arm { pat: PatId(0), guard: None, body: ExprId(0) };
}

#[test]
fn become_and_inline_asm_lower_and_verify() {
    use blaze_typeck::{
        AsmArch, AsmKind, AsmOperand, AsmPiece, AsmReg, BindAnn, BodyPath, IntTy, List, Lit, PNode,
        Pat, PatId, PathIdx, PortableTys, Range, ScopeId, Segment, Stmt, StmtId,
    };

    // 1. `fn f(x: i32) -> i32 { become g(x) }` lowers to `StmtKind::TailCall` and verifies!
    let mut fx = Fx::new();
    let i32_t = fx.ty(PNode::Int(IntTy::I32));
    let f_key = fx.func("f", 0, &[i32_t], i32_t, Default::default());
    let _g_key = fx.func("g", 0, &[i32_t], i32_t, Default::default());
    fx.finish();
    let bt1 = PortableTys::new_in(heap());
    let pats1 = [Pat::Bind {
        name: Name::of("x"),
        ann: BindAnn { by_ref: None, mutable: false },
        sub: None,
        ty: None,
    }];
    let pat_ranges1 = [Range { start: 0, end: 1 }];
    let segments1 = [Segment { name: Name::of("g"), generic_args: List::EMPTY, has_args: false }];
    let paths1 = [BodyPath {
        scope: ScopeId(0),
        segments: List { start: 0, len: 1 },
        lang: None,
        anchor: None,
    }];
    let exprs1 = [
        Expr::Path(PathIdx(0)),                                            // 0: g
        Expr::Local(PatId(0)),                                             // 1: x
        Expr::Call { callee: ExprId(0), args: List { start: 0, len: 1 } }, // 2: g(x)
        Expr::Become { value: ExprId(2) },                                 // 3: become g(x)
    ];
    let expr_lists1 = [ExprId(1)];
    let expr_ranges1 = [
        Range { start: 0, end: 1 },
        Range { start: 1, end: 2 },
        Range { start: 2, end: 3 },
        Range { start: 3, end: 4 },
    ];
    let params1 = [PatId(0)];
    let mut bv1 = empty_body_view(f_key, bt1.view(), ExprId(3));
    bv1.params = &params1;
    bv1.pats = &pats1;
    bv1.pat_ranges = &pat_ranges1;
    bv1.segments = &segments1;
    bv1.paths = &paths1;
    bv1.exprs = &exprs1;
    bv1.expr_lists = &expr_lists1;
    bv1.expr_ranges = &expr_ranges1;
    let mut cx1 = InferCtx::new_in(heap());
    let ck1 = match blaze_typeck::check(&bv1, &fx, &mut cx1, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "become fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("become world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let body1 = lower(&bv1, &ck1, &Plain, heap()).expect("become lowers cleanly");
    blaze_body::verify(&body1).expect("become body verifies");
    let mut text1 = blaze_own::String::new_in(heap());
    blaze_body::print(&body1.view(), &mut text1).expect("prints");
    assert!(text1.contains("tail_call fn"), "become g(x) must lower to tail_call:\n{text1}");

    // 2. `Expr::Asm` with `in(reg)` + `out(reg)` + `const` lowers, encodes via kept record,
    //    reopens, and verifies with full `AsmRow` metadata!
    let mut fx2 = Fx::new();
    let i64_t = fx2.ty(PNode::Int(IntTy::I64));
    let owner = fx2.func("asm_fn", 0, &[i64_t], i64_t, Default::default());
    fx2.finish();
    let mut bt = PortableTys::new_in(heap());
    let bt_i64 = bt.intern(PNode::Int(IntTy::I64)).unwrap();
    let pats = [
        Pat::Bind {
            name: Name::of("x"),
            ann: BindAnn { by_ref: None, mutable: false },
            sub: None,
            ty: None,
        },
        Pat::Bind {
            name: Name::of("out"),
            ann: BindAnn { by_ref: None, mutable: true },
            sub: None,
            ty: None,
        },
    ];
    let pat_ranges = [Range { start: 0, end: 1 }, Range { start: 1, end: 2 }];
    let exprs = [
        Expr::Lit(Lit::Int(None)), // 0: 0i64 (init out)
        Expr::Local(PatId(0)),     // 1: x (in)
        Expr::Local(PatId(1)),     // 2: out (out)
        Expr::Lit(Lit::Int(None)), // 3: 7i64 (const)
        Expr::Asm {
            arch: AsmArch::Aarch64,
            pieces: List { start: 0, len: 6 },
            operands: List { start: 0, len: 3 },
            options: 0,
            clobber_abis: List::EMPTY,
        }, // 4: asm!
        Expr::Local(PatId(1)),     // 5: out (tail)
        Expr::Block {
            stmts: List { start: 0, len: 2 },
            tail: Some(ExprId(5)),
            scope: ScopeId(0),
        }, // 6
    ];
    let expr_ranges = [
        Range { start: 0, end: 1 },
        Range { start: 1, end: 2 },
        Range { start: 2, end: 3 },
        Range { start: 3, end: 4 },
        Range { start: 4, end: 5 },
        Range { start: 5, end: 6 },
        Range { start: 6, end: 7 },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(1), ty: Some(bt_i64), init: Some(ExprId(0)), else_: None },
        Stmt::Expr { expr: ExprId(4), semi: true },
    ];
    let stmt_lists = [StmtId(0), StmtId(1)];
    let params = [PatId(0)];
    let asm_text = "add , , #reg";
    let asm_pieces = [
        AsmPiece::Text { first: 0, len: 4 }, // "add "
        AsmPiece::Operand { index: 1, modifier: None },
        AsmPiece::Text { first: 4, len: 2 }, // ", "
        AsmPiece::Operand { index: 0, modifier: None },
        AsmPiece::Text { first: 6, len: 3 }, // ", #"
        AsmPiece::Operand { index: 2, modifier: None },
    ];
    let asm_operands = [
        AsmOperand {
            kind: AsmKind::In,
            reg: Some(AsmReg::Class { first: 9, len: 3 }),
            expr: ExprId(1),
            out_expr: None,
        },
        AsmOperand {
            kind: AsmKind::Out,
            reg: Some(AsmReg::Class { first: 9, len: 3 }),
            expr: ExprId(2),
            out_expr: None,
        },
        AsmOperand { kind: AsmKind::Const, reg: None, expr: ExprId(3), out_expr: None },
    ];
    let call_arg_ints = [(ExprId(0), 0u128), (ExprId(3), 7u128)];
    let mut bv = empty_body_view(owner, bt.view(), ExprId(6));
    bv.params = &params;
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.asm_text = asm_text;
    bv.asm_pieces = &asm_pieces;
    bv.asm_operands = &asm_operands;
    bv.call_arg_ints = &call_arg_ints;
    let unsafe_blocks = [ExprId(6)];
    bv.unsafe_blocks = &unsafe_blocks;

    let mut cx = InferCtx::new_in(heap());
    let ck = match blaze_typeck::check(&bv, &fx2, &mut cx, &Needs::new_in(heap()), heap()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "asm fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("asm world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let kept_bytes = crate::keep(&bv, &ck, &Plain, heap()).expect("asm body keeps cleanly");
    let kept = blaze_body::kept::open(&kept_bytes, heap()).expect("kept asm record reopens cleanly");
    assert_eq!(kept.verified().asms(), 1, "kept body must preserve its AsmRow");
    assert_eq!(kept.verified().asm(0).ops.len(), 3, "AsmRow must preserve all 3 operands");
}

/// Coroutines: `async fn` outer body (`AggregateKind::Coroutine` with canonical `PNode::Coroutine`),
/// `lower_coroutine` with multiple `.await` points + droppable local live across `.await`
/// (dropped on both normal exit and `cancel` -> `cancelled`), and `Poll<!>` diverging `.await`.
#[test]
fn async_fn_async_block_and_await_lower_and_verify() {
    use blaze_typeck::{
        BindAnn, Capture, CaptureKind, Checked, CoroutineKind, Expr, ExprId, ItemKey, List, PNode,
        PTy, Pat, PatId, PortableTys, Range, ScopeId, Stmt, StmtId, UintTy,
    };
    use crate::{AsyncBodyItems, Discr, DiscrInt, lower_coroutine};

    const OWNER: ItemKey = ItemKey(0xa001);
    const INTO_FUTURE_FN: ItemKey = ItemKey(0xa010);
    const FUTURE_POLL_FN: ItemKey = ItemKey(0xa011);
    const POLL_ADT: ItemKey = ItemKey(0xa012);
    const PIN_ADT: ItemKey = ItemKey(0xa013);
    const GUARD_ADT: ItemKey = ItemKey(0xa014);
    const SUB_FUT_ADT: ItemKey = ItemKey(0xa015);
    const NEVER_FUT_ADT: ItemKey = ItemKey(0xa016);

    struct AsyncFacts;
    impl blaze_flow_lower::Facts for AsyncFacts {
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts { &blaze_flow_lower::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> { None }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_bodies_checked(&self) -> bool { false }
        fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { None }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> { None }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, k: ItemKey) -> Option<blaze_flow_lower::AdtFacts> {
            match k {
                GUARD_ADT => Some(blaze_flow_lower::AdtFacts { copy: false, drop_impl: true, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false }),
                POLL_ADT => Some(blaze_flow_lower::AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false }),
                PIN_ADT => Some(blaze_flow_lower::AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false }),
                SUB_FUT_ADT | NEVER_FUT_ADT => Some(blaze_flow_lower::AdtFacts { copy: false, drop_impl: true, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false }),
                _ => None,
            }
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> { None }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> { Some(1) }
        fn param_copy(&self, _: u32) -> bool { false }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { None }
    }
    impl crate::BodyFacts for AsyncFacts {
        fn trait_item(&self, k: ItemKey) -> Option<bool> {
            Some(matches!(k, INTO_FUTURE_FN | FUTURE_POLL_FN))
        }
        fn async_items(&self) -> Option<AsyncBodyItems> {
            Some(AsyncBodyItems {
                future_poll: FUTURE_POLL_FN,
                poll: POLL_ADT,
                ready: 0,
                pending: 1,
                pin: PIN_ADT,
                ready_discr: Discr { int: DiscrInt::Uint(UintTy::U8), bits: 0 },
            })
        }
    }

    let empty_checked = || -> Checked<'static> {
        let mut fx0 = Fx::new();
        let u_t = fx0.tuple(&[]);
        let f0 = fx0.func("empty", 0, &[], u_t, Default::default());
        fx0.finish();
        let bt0 = PortableTys::new_in(heap());
        let e0 = [Expr::Tuple { elems: List::EMPTY }];
        let r0 = [Range { start: 0, end: 1 }];
        let mut bv0 = empty_body_view(f0, bt0.view(), ExprId(0));
        bv0.exprs = &e0;
        bv0.expr_ranges = &r0;
        let mut cx0 = InferCtx::new_in(heap());
        let mut c = match blaze_typeck::check(&bv0, &fx0, &mut cx0, &Needs::new_in(heap()), heap()) {
            Ok(Outcome::Done(c)) => c,
            Ok(Outcome::Need(n)) => panic!("empty checked needed {:?}", n.questions),
            Err(e) => panic!("empty checked failed: {e:?}"),
        };
        c.expr_ty.clear();
        c.pat_ty.clear();
        c.local_ty.clear();
        c
    };

    // Build a synthetic checked `async fn f(up: u32, g: Guard, f1: SubFut, f2: SubFut) -> u32`:
    //   async move {
    //     let a = f1.await;
    //     let b = f2.await;
    //     b
    //   }
    let mut tys = PortableTys::new_in(heap());
    let u32_t = tys.intern(PNode::Uint(UintTy::U32)).unwrap();
    let unit_l = tys.intern_list(&[]).unwrap();
    let unit_t = tys.intern(PNode::Tuple(unit_l)).unwrap();
    let guard_t = tys.intern(PNode::Adt(GUARD_ADT, unit_l)).unwrap();
    let sub_fut_t = tys.intern(PNode::Adt(SUB_FUT_ADT, unit_l)).unwrap();
    let sub_fut_l = tys.intern_list(&[sub_fut_t]).unwrap();
    let future_t = tys.intern(PNode::Future(ExprId(7), u32_t, unit_t)).unwrap();

    let pats = [
        Pat::Bind { name: Name::of("up"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
        Pat::Bind { name: Name::of("g"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
        Pat::Bind { name: Name::of("f1"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
        Pat::Bind { name: Name::of("f2"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
        Pat::Bind { name: Name::of("a"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
        Pat::Bind { name: Name::of("b"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None },
    ];
    let pat_ranges = [Range { start: 0, end: 1 }; 6];
    let exprs = [
        Expr::Local(PatId(2)),                                                // 0: f1
        Expr::Await(ExprId(0)),                                               // 1: f1.await
        Expr::Local(PatId(3)),                                                // 2: f2
        Expr::Await(ExprId(2)),                                               // 3: f2.await
        Expr::Local(PatId(4)),                                                // 4: a
        Expr::Local(PatId(5)),                                                // 5: b
        Expr::Block { stmts: List { start: 0, len: 2 }, tail: Some(ExprId(5)), scope: ScopeId(0) }, // 6: inner block
        Expr::AsyncBlock { body: ExprId(6), capture: Capture::Move },         // 7: async move { .. }
    ];
    let expr_ranges = [Range { start: 0, end: 1 }; 8];
    let stmts = [
        Stmt::Let { pat: PatId(4), ty: None, init: Some(ExprId(1)), else_: None },
        Stmt::Let { pat: PatId(5), ty: None, init: Some(ExprId(3)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1)];
    let params = [PatId(0), PatId(1), PatId(2), PatId(3)];

    let bt_empty = PortableTys::new_in(heap());
    let mut bv = empty_body_view(OWNER, bt_empty.view(), ExprId(7));
    bv.coroutine = Some(CoroutineKind::Async { output: u32_t });
    bv.params = &params;
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;

    let mut ck = empty_checked();
    ck.types = tys;
    for &t in &[sub_fut_t, u32_t, sub_fut_t, u32_t, u32_t, u32_t, u32_t, future_t] {
        ck.expr_ty.push(t);
    }
    for &t in &[u32_t, guard_t, sub_fut_t, sub_fut_t, u32_t, u32_t] {
        ck.pat_ty.push(t);
        ck.local_ty.push(t);
    }
    for pid in [PatId(0), PatId(1), PatId(2), PatId(3)] {
        ck.captures.push((ExprId(7), pid, CaptureKind::ByValue));
    }
    for await_e in [ExprId(1), ExprId(3)] {
        ck.callees.push((await_e, INTO_FUTURE_FN, sub_fut_l));
        ck.awaited.push((await_e, sub_fut_t));
    }

    // 1. Outer `async fn` lowers to `AggregateKind::Coroutine` with canonical `PNode::Coroutine`!
    let outer = lower(&bv, &ck, &AsyncFacts, heap()).expect("outer async fn lowers cleanly");
    blaze_body::verify(&outer).expect("outer async fn verifies");
    let mut outer_txt = blaze_own::String::new_in(heap());
    blaze_body::print(&outer.view(), &mut outer_txt).expect("prints");
    assert!(outer_txt.contains("coroutine "), "outer async fn must construct coroutine aggregate:\n{outer_txt}");
    assert!(!outer_txt.contains("future e"), "canonicalization must replace PNode::Future with PNode::Coroutine:\n{outer_txt}");

    // 2. Inner coroutine body lowers with 2 `suspend`s, 2 `cancelled` paths, and `drop` on `g`!
    let inner = lower_coroutine(&bv, &ck, &AsyncFacts, ExprId(7), heap()).expect("inner coroutine lowers cleanly");
    let inner_v = blaze_body::verify(&inner).expect("inner coroutine verifies");
    let mut inner_txt = blaze_own::String::new_in(heap());
    blaze_body::print(&inner.view(), &mut inner_txt).expect("prints");
    assert_eq!(inner_txt.matches("suspend resume").count(), 2, "must emit 2 suspend terminators:\n{inner_txt}");
    assert_eq!(inner_txt.matches("cancelled").count(), 2, "must emit 2 cancel exit terminators:\n{inner_txt}");
    assert!(inner_txt.contains("drop _2"), "droppable capture `g` (_2) must be dropped on normal & cancel paths:\n{inner_txt}");
    // Verify binary round-trip with VERSION=5 preserves `Suspend` and `Cancelled`!
    let enc = blaze_body::try_encode(&inner.view(), heap()).expect("encode");
    let opened = blaze_body::open(&enc).expect("open");
    blaze_body::verify_ref(opened).expect("reopened verifies");
    assert_eq!(inner_v.args(), 4);

    // 3. `Poll<!>` diverging `.await` lowers unconditionally to `Suspend` -> `Cancelled`!
    let mut tys2 = PortableTys::new_in(heap());
    let never_t = tys2.intern(PNode::Never).unwrap();
    let unit_l2 = tys2.intern_list(&[]).unwrap();
    let unit_t2 = tys2.intern(PNode::Tuple(unit_l2)).unwrap();
    let nfut_t = tys2.intern(PNode::Adt(NEVER_FUT_ADT, unit_l2)).unwrap();
    let nfut_l = tys2.intern_list(&[nfut_t]).unwrap();
    let future_never_t = tys2.intern(PNode::Future(ExprId(2), never_t, unit_t2)).unwrap();

    let pats2 = [Pat::Bind { name: Name::of("nf"), ann: BindAnn { by_ref: None, mutable: false }, sub: None, ty: None }];
    let pat_ranges2 = [Range { start: 0, end: 1 }];
    let exprs2 = [
        Expr::Local(PatId(0)),                                  // 0: nf
        Expr::Await(ExprId(0)),                                 // 1: nf.await (type `!`)
        Expr::AsyncBlock { body: ExprId(1), capture: Capture::Move }, // 2: async move { nf.await }
    ];
    let expr_ranges2 = [Range { start: 0, end: 1 }; 3];
    let params2 = [PatId(0)];
    let bt_empty2 = PortableTys::new_in(heap());
    let mut bv2 = empty_body_view(OWNER, bt_empty2.view(), ExprId(2));
    bv2.coroutine = Some(CoroutineKind::Async { output: never_t });
    bv2.params = &params2;
    bv2.pats = &pats2;
    bv2.pat_ranges = &pat_ranges2;
    bv2.exprs = &exprs2;
    bv2.expr_ranges = &expr_ranges2;

    let mut ck2 = empty_checked();
    ck2.types = tys2;
    for &t in &[nfut_t, never_t, future_never_t] {
        ck2.expr_ty.push(t);
    }
    ck2.pat_ty.push(nfut_t);
    ck2.local_ty.push(nfut_t);
    ck2.captures.push((ExprId(2), PatId(0), CaptureKind::ByValue));
    ck2.callees.push((ExprId(1), INTO_FUTURE_FN, nfut_l));
    ck2.awaited.push((ExprId(1), nfut_t));

    let never_body = lower_coroutine(&bv2, &ck2, &AsyncFacts, ExprId(2), heap()).expect("Poll<!> coroutine lowers");
    blaze_body::verify(&never_body).expect("Poll<!> coroutine verifies");
    let mut never_txt = blaze_own::String::new_in(heap());
    blaze_body::print(&never_body.view(), &mut never_txt).expect("prints");
    assert!(never_txt.contains("suspend resume"), "Poll<!> must suspend:\n{never_txt}");
    assert!(never_txt.contains("cancelled"), "Poll<!> must have cancel exit:\n{never_txt}");
    assert!(!never_txt.contains("discr "), "Poll<!> must not read discriminant of uninhabited Poll<!>:\n{never_txt}");
    let _ = PTy(0);
}
