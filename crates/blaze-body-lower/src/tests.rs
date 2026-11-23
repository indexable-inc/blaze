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
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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
        closure_static_params: &[],
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
