//! Bodies built by hand as columns, checked by blaze-typeck over a small world, built, verified
//! and run on blaze-lir's evaluator.

use std::alloc::Global;
use std::format;
use std::string::String;

use blaze_lir::Ty;
use blaze_lir::eval::{Outcome as Run, Val};
use blaze_lir::func::SigId;
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_typeck::InferCtx;
use blaze_typeck::{
    Adt, AdtKind, Arm, AssocItem, BinOp, BindAnn, BodyPath, BodyView, CtorKind, Def, Edition, Expr, ExprId, Fetch,
    Field, FieldInit, FieldPat, FnSig, Hidden, ImplHeader, IntSuffix, IntTy, ItemKey, LangItem, List, Lit, Mutbl, Name,
    Needs, Outcome, PClause, PNode, PTy, PTys, PatId, PathIdx, PathRes, Pat, PortableTys, Predicates, Range, ScopeId,
    Seal, Segment, Shape, Stmt, StmtId, TypeAttrs, UintTy, Variant, World,
};

use crate::data::{DataAlloc, DataRecord};
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, BuildError, Built, Instance, Intrinsic, StaticItem, Subst};

/// A body under construction, with the literal values its form does not hold.
struct BodyBuf {
    exprs: Vec<Expr, Global>,
    expr_ranges: Vec<Range, Global>,
    pats: Vec<Pat, Global>,
    pat_ranges: Vec<Range, Global>,
    pat_lists: Vec<PatId, Global>,
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
    strs: Vec<(ExprId, &'static str), Global>,
    /// String literal patterns' texts (`Adts::str_pat`).
    pat_strs: Vec<(PatId, &'static str), Global>,
    /// Struct patterns' `name: pat` fields (`BodyView::field_pats`).
    field_pats: Vec<FieldPat, Global>,
    /// `asm!`s' template pieces, operands and text (`BodyView::asm_*`; tests/asm.rs).
    asm_pieces: Vec<blaze_typeck::AsmPiece, Global>,
    asm_operands: Vec<blaze_typeck::AsmOperand, Global>,
    asm_text: &'static str,
    /// Call arguments' literal values for `#[rustc_legacy_const_generics]` (`BodyView::call_arg_ints`).
    call_arg_ints: Vec<(ExprId, u128), Global>,
    /// Every expression made, as `BodyView::unsafe_blocks`: a test body is checked as written
    /// inside `unsafe`, so its intrinsic calls and `asm!`s need no block (the unsafety check's
    /// own tests are blaze-ladder's tests/front2_unsafety.rs).
    unsafe_blocks: Vec<ExprId, Global>,
}

impl BodyBuf {
    fn new() -> BodyBuf {
        BodyBuf {
            exprs: Vec::new_in(Global),
            expr_ranges: Vec::new_in(Global),
            pats: Vec::new_in(Global),
            pat_ranges: Vec::new_in(Global),
            pat_lists: Vec::new_in(Global),
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
            strs: Vec::new_in(Global),
            pat_strs: Vec::new_in(Global),
            field_pats: Vec::new_in(Global),
            asm_pieces: Vec::new_in(Global),
            asm_operands: Vec::new_in(Global),
            asm_text: "",
            call_arg_ints: Vec::new_in(Global),
            unsafe_blocks: Vec::new_in(Global),
        }
    }

    fn e(&mut self, e: Expr) -> ExprId {
        let at = self.exprs.len() as u32;
        self.unsafe_blocks.push(ExprId(at));
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
        self.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: true }, name: Name::of(name), sub: None, ty: None })
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
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: 1 }, lang: None, anchor: None });
        PathIdx(at)
    }

    fn exprs_list(&mut self, items: &[ExprId]) -> List {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.extend_from_slice(items);
        List { start, len: items.len() as u32 }
    }

    fn pats_list(&mut self, items: &[PatId]) -> List {
        let start = self.pat_lists.len() as u32;
        self.pat_lists.extend_from_slice(items);
        List { start, len: items.len() as u32 }
    }

    /// `(elems..)`.
    fn tuple(&mut self, elems: &[ExprId]) -> ExprId {
        let elems = self.exprs_list(elems);
        self.e(Expr::Tuple { elems })
    }

    /// `[elems..]`.
    fn array(&mut self, elems: &[ExprId]) -> ExprId {
        let elems = self.exprs_list(elems);
        self.e(Expr::Array { elems })
    }

    /// `base.index`.
    fn tfield(&mut self, base: ExprId, index: u32) -> ExprId {
        self.e(Expr::TupleField { base, index })
    }

    /// `base.name`.
    fn field(&mut self, base: ExprId, name: &str) -> ExprId {
        self.e(Expr::Field { base, name: Name::of(name) })
    }

    /// `base[index]`.
    fn index(&mut self, base: ExprId, index: ExprId) -> ExprId {
        self.e(Expr::Index { base, index })
    }

    /// `name(args..)`.
    fn call(&mut self, name: &str, args: &[ExprId]) -> ExprId {
        let path = self.path(name);
        let callee = self.e(Expr::Path(path));
        let args = self.exprs_list(args);
        self.e(Expr::Call { callee, args })
    }

    /// `while cond { body.. }` as `loop { if cond { body.. } else { break } }`.
    fn while_(&mut self, cond: ExprId, body: &[Stmt]) -> ExprId {
        let lp = self.e(Expr::Missing);
        let then = self.block(body, None);
        let brk = self.e(Expr::Break { value: None, target: lp });
        let else_ = self.block(&[], Some(brk));
        let iff = self.e(Expr::If { cond, then, else_: Some(else_) });
        let body = self.block(&[], Some(iff));
        self.set(lp, Expr::Loop { body });
        lp
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
            pat_lists: &self.pat_lists,
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &self.field_inits,
            field_pats: &self.field_pats,
            paths: &self.paths,
            segments: &self.segments,
            segment_ranges: &[],
            types: self.types.view(),
            type_lists: &[],
            type_list_ranges: &[],
            method_name_ranges: &[],
            opt_types: &[],
            fmt_uses: &[],
            coroutine: None,
            super_lets: &[],
            asm_pieces: &self.asm_pieces,
            asm_operands: &self.asm_operands,
            asm_clobbers: &[],
            asm_text: self.asm_text,
            call_arg_ints: &self.call_arg_ints,
            legacy_args: &[],
            unsafe_blocks: &self.unsafe_blocks,
            // A test body is one edition's text: no word of it was written by another's macro.
            expr_editions: &[],
            pat_editions: &[],
            record_expr_schemas: &[],
            closure_static_params: &[],
        }
    }
}

struct Fun {
    key: ItemKey,
    name: Name,
    /// Its own generic parameters, `Param(0..generics)` in its types.
    generics: u32,
    inputs: Vec<PTy, Global>,
    output: PTy,
    /// Its `#[rustc_legacy_const_generics(..)]` positions (`FnSig::legacy_const_generics`,
    /// `Adts::legacy_const_generics`); empty for most.
    legacy_const_generics: Vec<u32, Global>,
}

struct Struct {
    key: ItemKey,
    kind: AdtKind,
    generics: u32,
    variants: Vec<Variant, Global>,
    fields: Vec<Field, Global>,
}

/// Fns and structs by one-segment name; one type table for all their types.
struct TestWorld {
    types: PortableTys<'static>,
    /// What [`Adts::panic`] answers (tests/unwind.rs sets `Unwind`).
    panic: crate::Panic,
    /// What [`Adts::overflow_checks`] answers.
    overflow: crate::OverflowChecks,
    /// What [`Adts::ub_checks`] answers.
    ub: crate::UbChecks,
    fns: Vec<Fun, Global>,
    structs: Vec<Struct, Global>,
    /// The fns declared `extern "C"`; every other is of the Rust ABI.
    c_abi: Vec<ItemKey, Global>,
    /// The fns declared `extern "rust-call"` (to the builder only: the checker sees them as
    /// Rust fns, which types their bodies and calls the same).
    rust_call: Vec<ItemKey, Global>,
    /// The `panic_bounds_check` lang item fn, if declared.
    bounds: Option<ItemKey>,
    /// The `drop_in_place` lang item fn, if declared.
    drop_in_place: Option<ItemKey>,
    /// The `panic_cannot_unwind` lang item fn, if declared (tests/nounwind.rs).
    cannot_unwind: Option<ItemKey>,
    /// Per struct with a `Drop` impl: the struct, and the `Drop::drop` fn its drop calls.
    drops: Vec<(ItemKey, ItemKey), Global>,
    /// The fns declared `#[rustc_intrinsic]`, and which intrinsic each is.
    intrinsics: Vec<(ItemKey, Intrinsic), Global>,
    /// The ADTs with `repr` attributes; every other is `repr(Rust)`.
    reprs: Vec<(ItemKey, ReprAttrs), Global>,
    /// The `static` items a path may name.
    statics: Vec<(ItemKey, StaticItem), Global>,
    /// The `const` items and their evaluated values (`None`: the evaluator gave none).
    consts: Vec<(ItemKey, Option<DataRecord<Global>>), Global>,
    /// The promoted borrows and their evaluated values (`None`: the evaluator gave none).
    promoted: Vec<(ExprId, Option<DataRecord<Global>>), Global>,
    /// Every item's const generic parameters (`World::const_params`), by index, their types of
    /// this world's table, and the one-segment names that resolve to them as `Def::TyParam`
    /// (tests/const_paths.rs).
    const_params: Vec<(u32, PTy), Global>,
    param_names: Vec<(Name, u32), Global>,
    /// What `Adts::const_fn_instance` answers: the instance a constant's fn relocation names.
    const_fns: Vec<(crate::InstanceKey, Instance), Global>,
    /// `#[lang = "deref"] trait Deref { type Target; }` (`DEREF`) and its one impl (`DEREF_IMPL`):
    /// `impl Deref for <self> { type Target = <target>; }`, both types of this world's table.
    deref: Option<(PTy, PTy)>,
    /// The trait's associated type, and the impl's `Target`.
    deref_defs: Vec<blaze_typeck::AssocTyDef, Global>,
    deref_assoc: Vec<(Name, PTy), Global>,
    /// What `Adts::deref_fn(Not)` answers: the fn a call at `[Self]` runs.
    deref_fn: Option<ItemKey>,
    /// The ADT `Adts::lang_adt(ManuallyDrop)` answers.
    manually_drop: Option<ItemKey>,
    /// The ADT `Adts::lang_adt(OwnedBox)` answers (tests/box_moves.rs).
    owned_box: Option<ItemKey>,
    /// The fixture solver of `LayoutFacts::normalize`: `<self_ as Deref>::Target = target`,
    /// both types of this world's table (tests/projections.rs).
    projs: Vec<(PTy, PTy), Global>,
    /// A fn's where-clauses (`T: Deref`), their types of this world's table.
    where_clauses: Vec<(ItemKey, Vec<PClause, Global>), Global>,
    /// `#[lang = "index"] trait Index<Idx> { type Output; }` (`INDEX`) and its one impl
    /// (`INDEX_IMPL`): `impl Index<idx> for <self> { type Output = <out>; }`: the self type and
    /// the trait's arguments after `Self` (`[idx]`), of this world's table (tests/index.rs).
    index: Option<(PTy, blaze_typeck::ArgsAfterSelf)>,
    /// The trait's associated type, and the impl's `Output`.
    index_defs: Vec<blaze_typeck::AssocTyDef, Global>,
    index_assoc: Vec<(Name, PTy), Global>,
    /// What `Adts::index_fn(Not)` answers: the fn a call at `[Self, Idx]` runs.
    index_fn: Option<ItemKey>,
    /// `#[lang = "index_mut"] trait IndexMut<Idx>: Index<Idx> {}` (`INDEX_MUT`) and its one
    /// impl (`INDEX_MUT_IMPL`) for the `Index` impl's self type and index, when set: what
    /// `Adts::index_fn(Mut)` answers (tests/index.rs).
    index_mut_fn: Option<ItemKey>,
    /// Inherent methods `impl <ADT> { fn <name>(&mut self, ..) }` (`TestWorld::method`): the
    /// ADT's shape, the name, the item as `inherent` answers it, and the impl's self type.
    methods: Vec<(Shape, Name, AssocItem, PTy), Global>,
    /// The bug a control plants in the walk (`Adts::planted`).
    planted: crate::Planted,
    /// The fns declared `#[track_caller]` (`Adts::track_caller`; tests/track_caller.rs).
    tracked: Vec<ItemKey, Global>,
    /// What `Adts::location_adt` answers; when set, `Adts::caller_site` answers
    /// [`TRACK_SITE`] for every expression.
    location: Option<ItemKey>,
    /// What `Adts::fn_trait_method` answers for each of `Fn`, `FnMut`, `FnOnce`: the item a
    /// fn shim's body is named by (tests/track_caller.rs).
    fn_trait_method: Option<ItemKey>,
    /// What `Adts::def_site` answers for every item (tests/track_caller.rs).
    def_site: Option<crate::CallerSite<'static>>,
}

/// Where every expression is, to `Adts::caller_site` (tests/track_caller.rs).
const TRACK_SITE: crate::CallerSite<'static> = crate::CallerSite { file: "src/main.rs", item: MAIN, offset: 30 };

const DEREF: ItemKey = ItemKey(90);
const DEREF_TARGET: ItemKey = ItemKey(91);
const DEREF_IMPL: ItemKey = ItemKey(92);
const INDEX: ItemKey = ItemKey(93);
const INDEX_OUTPUT: ItemKey = ItemKey(94);
const INDEX_IMPL: ItemKey = ItemKey(95);
const INDEX_MUT: ItemKey = ItemKey(97);
const INDEX_MUT_IMPL: ItemKey = ItemKey(98);
/// What [`TestAdts`] answers for every `panic_const_*` lang fn (walk/arith_checks.rs).
const PANIC_CONST: ItemKey = ItemKey(96);

impl TestWorld {
    fn new() -> TestWorld {
        TestWorld {
            types: PortableTys::new_in(Alloc::global()),
            panic: crate::Panic::Abort,
            // The fixture's bodies were written for wrapping arithmetic (the release setting).
            overflow: crate::OverflowChecks::Off,
            // As the record path's baseline settings (blaze-ladder code_records.rs), no debug
            // assertions.
            ub: crate::UbChecks::Off,
            fns: Vec::new_in(Global),
            structs: Vec::new_in(Global),
            c_abi: Vec::new_in(Global),
            rust_call: Vec::new_in(Global),
            bounds: None,
            drop_in_place: None,
            cannot_unwind: None,
            drops: Vec::new_in(Global),
            intrinsics: Vec::new_in(Global),
            reprs: Vec::new_in(Global),
            statics: Vec::new_in(Global),
            consts: Vec::new_in(Global),
            promoted: Vec::new_in(Global),
            const_params: Vec::new_in(Global),
            param_names: Vec::new_in(Global),
            const_fns: Vec::new_in(Global),
            deref: None,
            deref_defs: Vec::new_in(Global),
            deref_assoc: Vec::new_in(Global),
            deref_fn: None,
            manually_drop: None,
            owned_box: None,
            projs: Vec::new_in(Global),
            where_clauses: Vec::new_in(Global),
            index: None,
            index_defs: Vec::new_in(Global),
            index_assoc: Vec::new_in(Global),
            index_fn: None,
            index_mut_fn: None,
            methods: Vec::new_in(Global),
            planted: crate::Planted::Nothing,
            tracked: Vec::new_in(Global),
            location: None,
            fn_trait_method: None,
            def_site: None,
        }
    }

    fn ty(&mut self, n: PNode) -> PTy {
        self.types.intern(n).expect("a type interns")
    }

    /// The tuple of `elems`, a node of this world's table.
    fn tuple(&mut self, elems: &[PNode]) -> PNode {
        let mut tys = Vec::new_in(Global);
        for &n in elems {
            let t = self.ty(n);
            tys.push(t);
        }
        PNode::Tuple(self.types.intern_list(&tys).expect("a list interns"))
    }

    fn array(&mut self, elem: PNode, n: u64) -> PNode {
        let elem = self.ty(elem);
        PNode::Array(elem, self.types.usize_value(n).expect("a length interns"))
    }

    fn ref_(&mut self, m: Mutbl, to: PNode) -> PNode {
        PNode::Ref(m, self.ty(to))
    }

    /// `impl Deref for <self_> { type Target = <target>; }`, whose `deref` is fn `deref_fn`.
    fn deref_impl(&mut self, self_: PNode, target: PNode, deref_fn: ItemKey) {
        let s = self.ty(self_);
        let t = self.ty(target);
        self.deref = Some((s, t));
        self.deref_defs.push(blaze_typeck::AssocTyDef { name: Name::of("Target"), item: DEREF_TARGET, default: None });
        self.deref_assoc.push((Name::of("Target"), t));
        self.deref_fn = Some(deref_fn);
    }

    /// `impl Index<idx> for <self_> { type Output = <out>; }`, whose `index` is fn `index_fn`.
    fn index_impl(&mut self, self_: PNode, idx: PNode, out: PNode, index_fn: ItemKey) {
        let s = self.ty(self_);
        let i = self.ty(idx);
        let o = self.ty(out);
        let with_self = self.types.intern_list(&[s, i]).expect("a list interns");
        let after = blaze_typeck::ArgsAfterSelf::split(self.types.view(), with_self, s).expect("`Self` first");
        self.index = Some((s, after));
        self.index_defs.push(blaze_typeck::AssocTyDef { name: Name::of("Output"), item: INDEX_OUTPUT, default: None });
        self.index_assoc.push((Name::of("Output"), o));
        self.index_fn = Some(index_fn);
    }

    /// `impl IndexMut<idx> for <self_> {}` for the `Index` impl's types, whose `index_mut` is
    /// fn `index_mut_fn`.
    fn index_mut_impl(&mut self, index_mut_fn: ItemKey) {
        assert!(self.index.is_some(), "`IndexMut: Index`: the `Index` impl first");
        self.index_mut_fn = Some(index_mut_fn);
    }

    /// `impl <adt> { fn <name>(self: <fn's first input>, ..) }`: impl `imp` holding method
    /// `f`, a fn already declared with `func` (its first input the receiver).
    fn method(&mut self, adt: ItemKey, name: &str, imp: ItemKey, f: ItemKey) {
        let self_ty = self.ty(PNode::Adt(adt, List::EMPTY));
        let item = AssocItem { container: imp, item: f, kind: blaze_typeck::AssocKind::Fn { self_param: true } };
        self.methods.push((Shape::Adt(adt), Name::of(name), item, self_ty));
    }

    /// `#[lang = "panic_bounds_check"] fn panic_bounds_check(index: usize, len: usize) -> !`.
    fn bounds_fn(&mut self) -> ItemKey {
        self.func(5, "panic_bounds_check", &[USIZE, USIZE], PNode::Never);
        self.bounds = Some(ItemKey(5));
        ItemKey(5)
    }

    fn func(&mut self, key: u128, name: &str, inputs: &[PNode], output: PNode) {
        self.generic_func(key, name, 0, inputs, output);
    }

    fn generic_func(&mut self, key: u128, name: &str, generics: u32, inputs: &[PNode], output: PNode) {
        let mut ins = Vec::new_in(Global);
        for &n in inputs {
            let t = self.ty(n);
            ins.push(t);
        }
        let output = self.ty(output);
        self.fns.push(Fun { key: ItemKey(key), name: Name::of(name), generics, inputs: ins, output, legacy_const_generics: Vec::new_in(Global) });
    }

    fn strukt(&mut self, key: u128, name: &str, fields: &[(&str, PNode)]) {
        self.generic_strukt(key, name, 0, fields);
    }

    fn generic_strukt(&mut self, key: u128, name: &str, generics: u32, fields: &[(&str, PNode)]) {
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
        self.structs.push(Struct { key: ItemKey(key), kind: AdtKind::Struct, generics, variants, fields: fs });
    }

    /// A non-generic enum `key`; its variants are keys `key + 1 + i`, a tuple variant's key
    /// also its ctor fn (fields in, the enum out). Variants resolve by their own name, as if
    /// imported (`use E::*`).
    fn enum_(&mut self, key: u128, variants: &[(&str, CtorKind, &[(&str, PNode)])]) {
        let mut fs = Vec::new_in(Global);
        let mut vs = Vec::new_in(Global);
        for (i, &(name, ctor, fields)) in variants.iter().enumerate() {
            let start = fs.len() as u32;
            for &(n, t) in fields {
                let ty = self.ty(t);
                fs.push(Field { name: Name::of(n), ty });
            }
            let vkey = key + 1 + i as u128;
            vs.push(Variant { key: ItemKey(vkey), name: Name::of(name), ctor, fields: List { start, len: fs.len() as u32 - start } });
            if ctor == CtorKind::Tuple {
                let mut inputs = Vec::new_in(Global);
                inputs.extend(fields.iter().map(|&(_, t)| t));
                self.func(vkey, "(ctor)", &inputs, PNode::Adt(ItemKey(key), List::EMPTY));
            }
        }
        self.structs.push(Struct { key: ItemKey(key), kind: AdtKind::Enum, generics: 0, variants: vs, fields: fs });
    }

    fn sig(&self, key: ItemKey) -> Option<FnSig<'_>> {
        self.fns.iter().find(|f| f.key == key).map(|f| FnSig {
            parent_count: 0,
            own_count: f.generics,
            declared_count: f.generics,
            self_param: self.methods.iter().any(|m| m.2.item == key),
            header: blaze_typeck::FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &f.legacy_const_generics,
            inputs: &f.inputs,
            output: f.output,
            types: self.types.view(),
        })
    }

    fn adt_of(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.structs.iter().find(|s| s.key == key).map(|s| Adt {
            kind: s.kind,
            generics: s.generics,
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

    fn closure(&self, _: Seal, _: ItemKey, _: u32) -> Fetch<Option<blaze_typeck::ClosureFacts<'_>>> {
        Ok(None)
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        match self.where_clauses.iter().find(|(k, _)| *k == item) {
            Some((_, clauses)) => Ok(Predicates { clauses: clauses.as_slice(), types: self.types.view() }),
            None => Ok(Predicates { clauses: &[] as &[PClause], types: PTys::EMPTY }),
        }
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        Ok(self.adt_of(item))
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        // Whether the impl's self type `s` is an ADT of `shape`.
        let ours = |s: Option<PTy>| match s.map(|s| self.types.view().node(s)) {
            Some(Ok(PNode::Adt(k, _))) => shape == Shape::Adt(k),
            _ => false,
        };
        Ok(if trait_ == DEREF && ours(self.deref.map(|(s, _)| s)) {
            &[DEREF_IMPL]
        } else if trait_ == INDEX && ours(self.index.map(|(s, _)| s)) {
            &[INDEX_IMPL]
        } else if trait_ == INDEX_MUT && self.index_mut_fn.is_some() && ours(self.index.map(|(s, _)| s)) {
            &[INDEX_MUT_IMPL]
        } else {
            &[]
        })
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        if let Some(m) = self.methods.iter().find(|m| m.2.container == item) {
            return Ok(Some(ImplHeader {
                generics: 0,
                self_ty: m.3,
                trait_: None,
                trait_args: blaze_typeck::ArgsAfterSelf::NONE,
                assoc_types: &[],
                types: self.types.view(),
                polarity: blaze_typeck::ImplPolarity::Positive,
            }));
        }
        Ok(match (item, self.deref, self.index) {
            (DEREF_IMPL, Some((s, _)), _) => Some(ImplHeader {
                            generics: 0,
                            self_ty: s,
                            trait_: Some(DEREF),
                            trait_args: blaze_typeck::ArgsAfterSelf::NONE,
                            assoc_types: &self.deref_assoc,
                            types: self.types.view(),
                            polarity: blaze_typeck::ImplPolarity::Positive,
                        }),
            (INDEX_IMPL, _, Some((s, after))) => Some(ImplHeader {
                generics: 0,
                self_ty: s,
                trait_: Some(INDEX),
                trait_args: after,
                assoc_types: &self.index_assoc,
                types: self.types.view(),
                polarity: blaze_typeck::ImplPolarity::Positive,
            }),
            (INDEX_MUT_IMPL, _, Some((s, after))) if self.index_mut_fn.is_some() => Some(ImplHeader {
                generics: 0,
                self_ty: s,
                trait_: Some(INDEX_MUT),
                trait_args: after,
                assoc_types: &[],
                types: self.types.view(),
                polarity: blaze_typeck::ImplPolarity::Positive,
            }),
            _ => None,
        })
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        Ok(match self.methods.iter().find(|m| m.0 == shape && m.1 == name) {
            Some(m) => core::slice::from_ref(&m.2),
            None => &[],
        })
    }

    fn assoc_item(&self, _: Seal, _: ItemKey, _: Name) -> Fetch<Option<AssocItem>> {
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }

    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<blaze_typeck::PathAnswer> {
        // This world names one-segment paths only: a longer one is a miss it does not locate,
        // a one-segment name that is none is its first segment's.
        let [n] = segments else { return Ok(Err(blaze_typeck::PathMiss::Whole)) };
        if let Some(&(_, index)) = self.param_names.iter().find(|(p, _)| p == n) {
            return Ok(Ok(PathRes { def: Def::TyParam(index), consumed: 1 }));
        }
        if let Some(f) = self.fns.iter().find(|f| f.name == *n) {
            // A static or a const is a `Fun` of no inputs, for its type.
            let def = match (self.statics.iter().find(|(k, _)| *k == f.key), self.consts.iter().any(|(k, _)| *k == f.key)) {
                (Some(&(k, s)), _) => Def::Static(k, if s.mutable { Mutbl::Mut } else { Mutbl::Not }),
                (None, true) => Def::Const(f.key),
                (None, false) => Def::Fn(f.key),
            };
            return Ok(Ok(PathRes { def, consumed: 1 }));
        }
        for s in self.structs.iter().filter(|s| s.kind == AdtKind::Enum) {
            if let Some(v) = s.variants.iter().position(|v| v.name == *n) {
                let def = Def::Ctor { adt: s.key, variant: v as u32, kind: s.variants[v].ctor };
                return Ok(Ok(PathRes { def, consumed: 1 }));
            }
        }
        Ok(self
            .structs
            .iter()
            .find(|s| s.kind != AdtKind::Enum && s.variants[0].name == *n)
            .map(|s| PathRes { def: Def::Adt(s.key), consumed: 1 })
            .ok_or(blaze_typeck::PathMiss::FIRST))
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(match item {
            LangItem::Deref => self.deref.is_some().then_some(DEREF),
            LangItem::Index => self.index.is_some().then_some(INDEX),
            LangItem::IndexMut => self.index_mut_fn.is_some().then_some(INDEX_MUT),
            // forms.rs: `c"..."`'s referent, when the test declares it.
            LangItem::CStr => self.structs.iter().any(|s| s.key == forms::CSTR).then_some(forms::CSTR),
            _ => None,
        })
    }

    fn lang_path(&self, _: Seal, _: LangItem) -> Fetch<Option<Def>> {
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

    fn trait_def(&self, _: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::TraitDef<'_>>> {
        if item == INDEX && self.index.is_some() {
            // `trait Index<Idx>`: `Self` and `Idx`.
            return Ok(Some(blaze_typeck::TraitDef {
                generics: 2,
                flags: blaze_typeck::TraitFlags(0),
                lang: Some(LangItem::Index),
                supertraits: &[],
                members: blaze_typeck::TraitMembers::NotKnown,
                assoc_types: &self.index_defs,
                types: self.types.view(),
                defaults: blaze_typeck::TraitDefaults::Known(&[None]),
            }));
        }
        if item == INDEX_MUT && self.index_mut_fn.is_some() {
            // `trait IndexMut<Idx>: Index<Idx>`: `Self` and `Idx`; no goal here asks the
            // supertrait, so it is not listed.
            return Ok(Some(blaze_typeck::TraitDef {
                generics: 2,
                flags: blaze_typeck::TraitFlags(0),
                lang: Some(LangItem::IndexMut),
                supertraits: &[],
                members: blaze_typeck::TraitMembers::NotKnown,
                assoc_types: &[],
                types: self.types.view(),
                defaults: blaze_typeck::TraitDefaults::Known(&[None]),
            }));
        }
        Ok((item == DEREF && self.deref.is_some()).then(|| blaze_typeck::TraitDef {
            generics: 1,
            flags: blaze_typeck::TraitFlags(0),
            lang: Some(LangItem::Deref),
            supertraits: &[],
            members: blaze_typeck::TraitMembers::NotKnown,
            assoc_types: &self.deref_defs,
            types: self.types.view(),
            defaults: blaze_typeck::TraitDefaults::Known(&[]),
        }))
    }

    fn assoc_parent(&self, _: Seal, _: ItemKey) -> Fetch<Option<blaze_typeck::AssocParent>> {
        Ok(None)
    }

    /// No test body has a `where const` clause: a question about one is a test bug.
    fn const_holds(&self, _: Seal, _: ItemKey, _: &[PTy], _: PTys<'_>) -> Fetch<blaze_typeck::ConstHolds> {
        Ok(blaze_typeck::ConstHolds::Error)
    }

    /// Every item has the const parameters `const_params` lists (none but in
    /// tests/const_paths.rs).
    fn const_params(&self, _: Seal, _: ItemKey) -> Fetch<blaze_typeck::ConstParams<'_>> {
        Ok(blaze_typeck::ConstParams { params: &self.const_params, types: self.types.view() })
    }
}

/// The test world's layout facts: its ADTs, the `repr`s it lists, no explicit discriminant
/// and no valid-range attribute.
impl LayoutFacts for TestWorld {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.adt_of(key)
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(self.reprs.iter().find(|(k, _)| *k == key).map_or(ReprAttrs::RUST, |&(_, r)| r))
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }

    /// The fixture solver: `<args[0] as Deref>::Target` by the rows of `projs`, compared as
    /// ground types (the ground table interns, so equal types are equal ids).
    fn normalize(&self, trait_: ItemKey, name: Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let refused = BuildError::Projection { trait_, name };
        if trait_ != DEREF || name != Name::of("Target") || args.len() != 1 {
            return Err(refused);
        }
        for &(self_, target) in self.projs.iter() {
            if crate::ground(self.types.view(), self_, &[], ground, Alloc::global(), self)? == args[0] {
                return crate::ground(self.types.view(), target, &[], ground, Alloc::global(), self);
            }
        }
        Err(refused)
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

    fn facts(&self) -> &dyn LayoutFacts {
        self.world
    }

    fn panic(&self) -> crate::Panic {
        self.world.panic
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        self.world.overflow
    }

    fn ub_checks(&self) -> crate::UbChecks {
        self.world.ub
    }

    fn static_item(&self, item: ItemKey) -> Option<StaticItem> {
        self.world.statics.iter().find(|(k, _)| *k == item).map(|&(_, s)| s)
    }

    fn const_value(&self, item: ItemKey, _: &[PTy]) -> Option<&DataRecord<Global>> {
        self.world.consts.iter().find(|(k, _)| *k == item).and_then(|(_, r)| r.as_ref())
    }

    fn promoted_value(&self, _: ItemKey, borrow: ExprId, _: &[PTy]) -> Option<&DataRecord<Global>> {
        self.world.promoted.iter().find(|(k, _)| *k == borrow).and_then(|(_, r)| r.as_ref())
    }

    fn const_fn_instance(&self, key: crate::InstanceKey, _: &mut PortableTys<'_>) -> Option<Instance> {
        self.world.const_fns.iter().find(|(k, _)| *k == key).map(|&(_, i)| i)
    }

    fn int_lit(&self, _: ItemKey, expr: ExprId) -> Option<u128> {
        self.body.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn int_pat(&self, _: ItemKey, pat: PatId) -> Option<u128> {
        self.body.pat_lits.iter().find(|(p, _)| *p == pat).map(|&(_, v)| v)
    }

    /// tests/matches.rs: a float pattern's bits ride in `pat_lits` as an integer's do.
    fn float_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.int_pat(owner, pat)
    }

    fn str_pat(&self, _: ItemKey, pat: PatId) -> Option<&[u8]> {
        self.body.pat_strs.iter().find(|(p, _)| *p == pat).map(|&(_, s)| s.as_bytes())
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        match name {
            "panic_bounds_check" => self.world.bounds,
            "drop_in_place" => self.world.drop_in_place,
            "panic_cannot_unwind" => self.world.cannot_unwind,
            // The arithmetic checks' entry points (walk/arith_checks.rs): one fn for all; no
            // test of this world reaches one (tests/arith_panics.rs runs them).
            n if crate::PanicConst::ALL.iter().any(|p| p.lang() == n) => Some(PANIC_CONST),
            _ => None,
        }
    }

    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        match mutbl {
            Mutbl::Not => self.world.deref_fn,
            Mutbl::Mut => None,
        }
    }

    fn index_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        match mutbl {
            Mutbl::Not => self.world.index_fn,
            Mutbl::Mut => self.world.index_mut_fn,
        }
    }

    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        match item {
            LangItem::ManuallyDrop => self.world.manually_drop,
            LangItem::OwnedBox => self.world.owned_box,
            _ => None,
        }
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        let PNode::Adt(k, _) = ground.view().node(ty).map_err(|_| BuildError::Malformed { what: "a type past the ground table" })? else {
            return Ok(None);
        };
        Ok(self.world.drops.iter().find(|&&(s, _)| s == k).map(|&(_, f)| f))
    }

    fn str_lit(&self, _: ItemKey, expr: ExprId) -> Option<&str> {
        self.body.strs.iter().find(|(e, _)| *e == expr).map(|&(_, s)| s)
    }

    /// forms.rs: a float literal's bits ride in `lits` as an integer's value does.
    fn float_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.int_lit(owner, expr)
    }

    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        self.str_lit(owner, expr).map(str::as_bytes)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.world.intrinsics.iter().find(|(k, _)| *k == item).map(|&(_, i)| i)
    }

    fn planted(&self) -> crate::Planted {
        self.world.planted
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.world.fns.iter().any(|f| f.key == item).then(|| {
            if self.world.c_abi.contains(&item) {
                Abi::C
            } else if self.world.rust_call.contains(&item) {
                Abi::RustCall
            } else {
                Abi::Rust
            }
        })
    }

    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] {
        self.world.fns.iter().find(|f| f.key == item).map(|f| &f.legacy_const_generics[..]).unwrap_or(&[])
    }

    /// The world's `extern "C"` fns are "C" (non-unwinding); every other unwinds.
    fn nounwind(&self, item: ItemKey) -> Option<bool> {
        self.world.fns.iter().any(|f| f.key == item).then(|| self.world.c_abi.contains(&item))
    }

    fn track_caller(&self, instance: Instance, _: &mut PortableTys<'_>) -> Option<bool> {
        let item = instance.item;
        self.world.fns.iter().any(|f| f.key == item).then(|| self.world.tracked.contains(&item))
    }

    fn caller_site(&self, _: ItemKey, _: ExprId) -> Result<crate::CallerSite<'_>, crate::SiteGap> {
        self.world.location.map(|_| TRACK_SITE).ok_or(crate::SiteGap::Unanswered)
    }

    fn def_site(&self, _: ItemKey) -> Result<crate::CallerSite<'_>, crate::SiteGap> {
        self.world.def_site.ok_or(crate::SiteGap::Unanswered)
    }

    fn fn_trait_method(&self, _: crate::FnTrait) -> Option<ItemKey> {
        self.world.fn_trait_method
    }

    fn location_adt(&self) -> Option<ItemKey> {
        self.world.location
    }
}

const MAIN: ItemKey = ItemKey(1);

/// Checks and builds `body` of `MAIN`, a non-generic fn: the instance with no arguments.
fn build(world: &TestWorld, body: &BodyBuf, root: ExprId) -> Result<Built<Global>, String> {
    let mut ground = PortableTys::new_in(Alloc::global());
    build_instance(world, body, MAIN, root, &[], &mut ground)
}

/// Checks `body` of `owner` once, generically, and builds its instance at `args` (types of
/// `ground`).
fn build_instance(
    world: &TestWorld,
    body: &BodyBuf,
    owner: ItemKey,
    root: ExprId,
    args: &[PTy],
    ground: &mut PortableTys<'static>,
) -> Result<Built<Global>, String> {
    build_instance_refusing(world, body, owner, root, args, ground, &[])
}

/// [`build_instance`], with the ops `refused` the baseline tier is expected to refuse
/// ([`emits`]; tests/asm.rs: `asm` goes to the Optimized tier).
fn build_instance_refusing(
    world: &TestWorld,
    body: &BodyBuf,
    owner: ItemKey,
    root: ExprId,
    args: &[PTy],
    ground: &mut PortableTys<'static>,
    refused: &[blaze_lir::Op],
) -> Result<Built<Global>, String> {
    let view = body.view(owner, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, world, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => return Err(format!("the check needed {:?}", n.questions)),
        Err(error) => return Err(format!("the check failed: {error:?}")),
    };
    if !checked.diags.is_empty() {
        return Err(format!("diagnostics: {:?}", checked.diags));
    }
    let sig = world.sig(owner).ok_or_else(|| String::from("no signature for the owner"))?;
    let mut subst = Subst { args, ground, scratch: Alloc::global() };
    let built = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world, body }, Global)
        .map_err(|error| format!("the build failed: {error}"))?;
    blaze_lir::verify(built.func.view()).map_err(|error| format!("does not verify: {error}"))?;
    emits(&built, refused);
    Ok(built)
}

/// Rule 54: every function the walk builds is also emitted by the baseline tier (blaze-emit,
/// each reference `Via::Address`), so a walk that makes an op the tier cannot emit fails
/// here, not in the ladder. `refused` names the ops a test expects the tier to refuse.
fn emits(built: &Built<Global>, refused: &[blaze_lir::Op]) {
    let mut vias = Vec::new_in(Global);
    vias.resize(built.refs.len(), blaze_emit::Via::Address);
    let mut code = blaze_emit::Code::new_in(Global);
    let isa = blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs };
    match blaze_emit::emit(built.func.verified(), &vias, isa, &mut code) {
        Ok(()) => {}
        Err(blaze_emit::EmitError::Unsupported { op, .. }) if refused.contains(&op) => {}
        Err(error) => panic!("the baseline tier refused the walk's function: {error}\n{}", blaze_lir::print(built.func.view())),
    }
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
    let lit = b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
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
    assert_eq!(built.refs.as_slice(), &[Instance { item: ItemKey(7), args: List::EMPTY }]);
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("ref ref0 = func 0"), "{text}");
    assert!(text.lines().any(|l| l.contains("call") && l.contains("ref0")), "{text}");
}

/// `#[rustc_legacy_const_generics(1)] fn vget<const N: i32>(v: i32) -> i32;`
/// `fn main() -> i32 { vget(5, 3) }`: the checker makes `3` the callee's const argument
/// (`vget::<3>`), so the call passes one value, `5`. A builder that passed every written
/// argument makes a call of two values and fails the last assertion.
///
/// Prints: `test tests::a_legacy_const_generic_argument_is_not_passed ... ok`
#[test]
fn a_legacy_const_generic_argument_is_not_passed() {
    const VGET: ItemKey = ItemKey(8);
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    w.generic_func(VGET.0, "vget", 1, &[I32], I32);
    w.fns.last_mut().expect("vget was just declared").legacy_const_generics.push(1);
    let i32_ = w.ty(I32);
    w.const_params.push((0, i32_));
    let mut b = BodyBuf::new();
    let path = b.path("vget");
    let callee = b.e(Expr::Path(path));
    let v = b.int(5, None);
    let lane = b.int(3, None);
    b.call_arg_ints.push((v, 5));
    b.call_arg_ints.push((lane, 3));
    let args = b.exprs_list(&[v, lane]);
    let root = b.e(Expr::Call { callee, args });
    let mut ground = PortableTys::new_in(Alloc::global());
    let built = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let [callee_instance] = built.refs.as_slice() else { panic!("one callee, not {:?}", built.refs) };
    assert_eq!(callee_instance.item, VGET);
    let gv = ground.view();
    let args = gv.list(callee_instance.args).expect("the callee's arguments are in the ground table");
    let [n] = args else { panic!("`vget` takes one generic argument, not {args:?}") };
    assert!(matches!(gv.node(*n), Ok(PNode::ConstValue { bits: 3, .. })), "`N` is 3: {:?}", gv.node(*n));
    let view = built.func.view();
    let text = blaze_lir::print(view);
    let mut passed = Vec::new_in(Global);
    for block in 0..view.blocks() {
        for at in view.block(blaze_lir::Block(block as u32)) {
            let op = view.op(at);
            if op == blaze_lir::Op::Call || op == blaze_lir::Op::Invoke {
                let mut o = view.operands(at);
                let _callee = o.reference();
                let _sig = o.sig();
                if op == blaze_lir::Op::Invoke {
                    let _normal = o.block();
                    let _pad = o.block();
                }
                passed.push(o.values().len());
            }
        }
    }
    assert_eq!(passed.as_slice(), &[1], "one call, of one value (`5`):\n{text}");
}

const U8: PNode = PNode::Uint(UintTy::U8);
const T: PNode = PNode::Param(0);
const PICK: ItemKey = ItemKey(2);

/// `fn pick<T>(c: bool, a: T, b: T) -> T { if c { a } else { b } }`, in `w` and as a body.
fn pick(w: &mut TestWorld) -> (BodyBuf, ExprId) {
    w.generic_func(2, "pick", 1, &[PNode::Bool, T, T], T);
    let mut b = BodyBuf::new();
    let c = b.bind("c");
    let a = b.bind("a");
    let bb = b.bind("b");
    b.params.extend_from_slice(&[c, a, bb]);
    let cond = b.local(c);
    let a_e = b.local(a);
    let then = b.block(&[], Some(a_e));
    let b_e = b.local(bb);
    let else_ = b.block(&[], Some(b_e));
    let root = b.e(Expr::If { cond, then, else_: Some(else_) });
    (b, root)
}

/// `pick` checked once, built at `i32` and at `u8`: two functions, each correct at its width.
#[test]
fn a_generic_fn_is_built_once_per_instance() {
    let mut w = TestWorld::new();
    let (b, root) = pick(&mut w);
    let mut g = PortableTys::new_in(Alloc::global());
    let i32_ = g.intern(I32).expect("a type interns");
    let u8_ = g.intern(U8).expect("a type interns");
    let at_i32 = build_instance(&w, &b, PICK, root, &[i32_], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let at_u8 = build_instance(&w, &b, PICK, root, &[u8_], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let (t32, t8) = (blaze_lir::print(at_i32.func.view()), blaze_lir::print(at_u8.func.view()));
    assert_ne!(t32, t8, "one function for two instances");
    assert_eq!(one(&at_i32, &[Val::Int(1), Val::Int(30), Val::Int(1)]), 30);
    assert_eq!(one(&at_i32, &[Val::Int(0), Val::Int(30), Val::Int(0xFFFF_FFFF)]), 0xFFFF_FFFF);
    assert_eq!(one(&at_u8, &[Val::Int(1), Val::Int(7), Val::Int(200)]), 7);
    assert_eq!(one(&at_u8, &[Val::Int(0), Val::Int(7), Val::Int(200)]), 200);
}

/// `struct Pair<T> { a: T, b: T }`; `fn second<T>(a: T, b: T) -> T { let p = Pair { a, b };
/// let q = Pair { a: p.b, b: p.a }; q.a }` at `u8` and at `i64`: `Pair` laid out per argument.
#[test]
fn a_generic_struct_is_laid_out_at_its_arguments() {
    let mut w = TestWorld::new();
    w.generic_func(3, "second", 1, &[T, T], T);
    w.generic_strukt(9, "Pair", 1, &[("a", T), ("b", T)]);
    let mut b = BodyBuf::new();
    let a = b.bind("a");
    let bb = b.bind("b");
    b.params.extend_from_slice(&[a, bb]);
    let p = b.bind("p");
    let q = b.bind("q");
    let pair = |b: &mut BodyBuf, x: ExprId, y: ExprId| {
        let path = b.path("Pair");
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("a"), expr: x });
        b.field_inits.push(FieldInit { name: Name::of("b"), expr: y });
        b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None })
    };
    let field = |b: &mut BodyBuf, of: PatId, name: &str| {
        let base = b.local(of);
        b.e(Expr::Field { base, name: Name::of(name) })
    };
    let a_e = b.local(a);
    let b_e = b.local(bb);
    let p_lit = pair(&mut b, a_e, b_e);
    let pb = field(&mut b, p, "b");
    let pa = field(&mut b, p, "a");
    let q_lit = pair(&mut b, pb, pa);
    let qa = field(&mut b, q, "a");
    let root = b.block(
        &[
            Stmt::Let { pat: p, ty: None, init: Some(p_lit), else_: None },
            Stmt::Let { pat: q, ty: None, init: Some(q_lit), else_: None },
        ],
        Some(qa),
    );
    let mut g = PortableTys::new_in(Alloc::global());
    let u8_ = g.intern(U8).expect("a type interns");
    let i64_ = g.intern(PNode::Int(IntTy::I64)).expect("a type interns");
    let at_u8 = build_instance(&w, &b, ItemKey(3), root, &[u8_], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let at_i64 = build_instance(&w, &b, ItemKey(3), root, &[i64_], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let largest = |built: &Built<Global>| built.func.view().slots.iter().map(|s| s.size).max();
    assert_eq!(largest(&at_u8), Some(2), "Pair<u8> is two bytes\n{}", blaze_lir::print(at_u8.func.view()));
    assert_eq!(largest(&at_i64), Some(16), "Pair<i64> is sixteen bytes\n{}", blaze_lir::print(at_i64.func.view()));
    assert_eq!(one(&at_u8, &[Val::Int(5), Val::Int(9)]), 9);
    assert_eq!(one(&at_i64, &[Val::Int(5), Val::Int(1 << 40)]), 1 << 40);
}

/// `fn main() -> u8 { pick(true, 1u8, 2u8) + pick(false, 3u8, 4u8) }`: one instance, one place.
#[test]
fn two_calls_of_one_instance_share_one_ref() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U8);
    pick(&mut w);
    let mut b = BodyBuf::new();
    let call = |b: &mut BodyBuf, c: bool, x: u128, y: u128| {
        let path = b.path("pick");
        let callee = b.e(Expr::Path(path));
        let c = b.e(Expr::Lit(Lit::Bool(c)));
        let x = b.int(x, Some(IntSuffix::Uint(UintTy::U8)));
        let y = b.int(y, Some(IntSuffix::Uint(UintTy::U8)));
        let args = b.exprs_list(&[c, x, y]);
        b.e(Expr::Call { callee, args })
    };
    let first = call(&mut b, true, 1, 2);
    let second = call(&mut b, false, 3, 4);
    let root = b.bin(BinOp::Add, first, second);
    let mut g = PortableTys::new_in(Alloc::global());
    let built = build_instance(&w, &b, MAIN, root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let u8_ = g.intern(U8).expect("a type interns");
    let args = g.intern_list(&[u8_]).expect("a list interns");
    assert_eq!(built.refs.as_slice(), &[Instance { item: PICK, args }]);
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

const U32: PNode = PNode::Uint(UintTy::U32);
const USIZE: PNode = PNode::Uint(UintTy::Usize);
const SU32: Option<IntSuffix> = Some(IntSuffix::Uint(UintTy::U32));

/// A signature's parameter and result types.
fn sig_tys(built: &Built<Global>, sig: SigId) -> (Vec<Ty, Global>, Vec<Ty, Global>) {
    let view = built.func.view();
    let s = view.sig(sig);
    let mut params = Vec::new_in(Global);
    params.extend(s.params.iter().map(|p| Ty(p.ty)));
    let mut rets = Vec::new_in(Global);
    rets.extend(s.rets.iter().map(|p| Ty(p.ty)));
    (params, rets)
}

/// The function's own signature.
fn own_sig(built: &Built<Global>) -> (Vec<Ty, Global>, Vec<Ty, Global>) {
    sig_tys(built, SigId(built.func.view().head.sig))
}

/// Whether some call in the function has signature `params -> rets`: the signature its call
/// instruction names. Signatures are made once however often asked for (blaze-lir builder.rs
/// `sig`), so a call whose signature equals the function's own (a tracked fn calling a tracked
/// fn of its shape) names the head's: the call's operand is read, never "every signature but
/// the head's".
fn has_call_sig(built: &Built<Global>, params: &[Ty], rets: &[Ty]) -> bool {
    use blaze_lir::Op;
    let view = built.func.view();
    (0..view.len()).any(|inst| {
        let mut o = view.operands(inst);
        let sig = match view.op(inst) {
            Op::Call | Op::Invoke | Op::TailCall => {
                let _callee = o.reference();
                o.sig()
            }
            Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
                let _callee = o.value();
                o.sig()
            }
            _ => return false,
        };
        let (p, r) = sig_tys(built, sig);
        p.as_slice() == params && r.as_slice() == rets
    })
}

/// `struct P { a: i32, b: u8 }` and `fn mk(a: i32, b: u8) -> P`, item 3, in `w`: the type `P`.
fn p_and_mk(w: &mut TestWorld) -> PNode {
    w.strukt(9, "P", &[("a", I32), ("b", U8)]);
    let p = PNode::Adt(ItemKey(9), List::EMPTY);
    w.func(3, "mk", &[I32, U8], p);
    p
}

/// `P { a: x, b: y }`.
fn p_lit(b: &mut BodyBuf, x: ExprId, y: ExprId) -> ExprId {
    let path = b.path("P");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: x });
    b.field_inits.push(FieldInit { name: Name::of("b"), expr: y });
    b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None })
}

/// `fn mk(a: i32, b: u8) -> P { P { a, b } }` writes its result through a hidden first
/// pointer and returns nothing; `fn main() -> i32 { let p = mk(40, 2); p.a + p.b as i32 }`
/// passes its slot's address first and reads the fields back from that slot.
#[test]
fn a_struct_is_returned_through_the_result_pointer() {
    let mut w = TestWorld::new();
    p_and_mk(&mut w);
    let mut callee = BodyBuf::new();
    let a = callee.bind("a");
    let bb = callee.bind("b");
    callee.params.extend_from_slice(&[a, bb]);
    let a_e = callee.local(a);
    let b_e = callee.local(bb);
    let root = p_lit(&mut callee, a_e, b_e);
    let mut g = PortableTys::new_in(Alloc::global());
    let mk = build_instance(&w, &callee, ItemKey(3), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(mk.func.view());
    assert_eq!(own_sig(&mk), (vec_of(&[Ty::PTR, Ty::I32, Ty::I8]), vec_of(&[])), "{text}");
    // The result pointer is the entry's first parameter, `v0`; both fields are stored through it.
    let through = text.lines().filter(|l| l.contains("store") && l.contains("v0")).count();
    assert_eq!(through, 2, "{text}");

    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let p = b.bind("p");
    let forty = b.int(40, None);
    let two = b.int(2, None);
    let call = b.call("mk", &[forty, two]);
    let pa = b.local(p);
    let pa = b.field(pa, "a");
    let pb = b.local(p);
    let pb = b.field(pb, "b");
    let i32_ty = b.types.intern(I32).expect("a type interns");
    let cast = b.e(Expr::Cast { operand: pb, ty: i32_ty });
    let sum = b.bin(BinOp::Add, pa, cast);
    let root = b.block(&[Stmt::Let { pat: p, ty: None, init: Some(call), else_: None }], Some(sum));
    let main = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(has_call_sig(&main, &[Ty::PTR, Ty::I32, Ty::I8], &[]), "{text}");
    // The binding lives in the result slot: one slot of P's 8 bytes, no copy into another.
    let eights = main.func.view().slots.iter().filter(|s| s.size == 8).count();
    assert_eq!(eights, 1, "{text}");
    // The evaluator does not model calls: the run stops at `mk`'s.
    let refused = run(&main, &[]).expect_err("the evaluator runs no call");
    assert!(refused.contains("op: Call"), "{refused}");
}

fn vec_of(tys: &[Ty]) -> Vec<Ty, Global> {
    let mut v = Vec::new_in(Global);
    v.extend_from_slice(tys);
    v
}

/// `fn swap(t: (u32, u32)) -> (u32, u32) { (t.1, t.0) }` takes its argument as a pointer and
/// its result pointer first; `let (x, y) = swap((1, 2)); x * 10 + y` passes the literal's own
/// temporary (no second copy) and takes the result apart with a tuple pattern.
#[test]
fn a_tuple_is_passed_and_returned_by_pointer() {
    let mut w = TestWorld::new();
    let pair = w.tuple(&[U32, U32]);
    w.func(4, "swap", &[pair], pair);
    let mut callee = BodyBuf::new();
    let t = callee.bind("t");
    callee.params.push(t);
    let t1 = callee.local(t);
    let t1 = callee.tfield(t1, 1);
    let t0 = callee.local(t);
    let t0 = callee.tfield(t0, 0);
    let root = callee.tuple(&[t1, t0]);
    let mut g = PortableTys::new_in(Alloc::global());
    let swap = build_instance(&w, &callee, ItemKey(4), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(swap.func.view());
    assert_eq!(own_sig(&swap), (vec_of(&[Ty::PTR, Ty::PTR]), vec_of(&[])), "{text}");
    // `t` is read in place from the caller's copy: no slot holds a second one.
    assert_eq!(swap.func.view().slots.len(), 1, "only the result literal's temporary\n{text}");

    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    let y = b.bind("y");
    let elems = b.pats_list(&[x, y]);
    let pat = b.pat(Pat::Tuple { elems, rest: None });
    let one_ = b.int(1, SU32);
    let two = b.int(2, SU32);
    let lit = b.tuple(&[one_, two]);
    let call = b.call("swap", &[lit]);
    let x_e = b.local(x);
    let ten = b.int(10, None);
    let tens = b.bin(BinOp::Mul, x_e, ten);
    let y_e = b.local(y);
    let sum = b.bin(BinOp::Add, tens, y_e);
    let root = b.block(&[Stmt::Let { pat, ty: None, init: Some(call), else_: None }], Some(sum));
    let main = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(has_call_sig(&main, &[Ty::PTR, Ty::PTR], &[]), "{text}");
    let tuples = main.func.view().slots.iter().filter(|s| s.size == 8).count();
    assert_eq!(tuples, 2, "the literal and the result, no copy of the literal\n{text}");
}

/// `let mut a = [1u32, 2, 3, 4]; a[1] = 10; a[2] += 5; (&mut a)[3] += 1;
/// let mut acc = (0u32, 0u32); let mut i = 0; while i < 4 { acc.0 += a[i]; acc.1 += 1; i += 1; }
/// let (s, n) = acc; let [_, _, c, _] = a; s * 100 + n * 10 + c`: 24 * 100 + 40 + 8.
#[test]
fn arrays_and_tuples_are_read_written_and_taken_apart() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.bounds_fn();
    let mut b = BodyBuf::new();
    let a = b.bind("a");
    let acc = b.bind("acc");
    let i = b.bind("i");
    let s = b.bind("s");
    let n = b.bind("n");
    let c = b.bind("c");
    let e1 = b.int(1, SU32);
    let e2 = b.int(2, None);
    let e3 = b.int(3, None);
    let e4 = b.int(4, None);
    let arr = b.array(&[e1, e2, e3, e4]);
    // a[1] = 10;
    let a_e = b.local(a);
    let k = b.int(1, None);
    let a1 = b.index(a_e, k);
    let ten = b.int(10, None);
    let set = b.e(Expr::Assign { lhs: a1, rhs: ten });
    // a[2] += 5;
    let a_e = b.local(a);
    let k = b.int(2, None);
    let a2 = b.index(a_e, k);
    let five = b.int(5, None);
    let add5 = b.e(Expr::AssignOp { op: BinOp::Add, lhs: a2, rhs: five });
    // (&mut a)[3] += 1;
    let a_e = b.local(a);
    let r = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: a_e });
    let k = b.int(3, None);
    let a3 = b.index(r, k);
    let one_ = b.int(1, None);
    let add1 = b.e(Expr::AssignOp { op: BinOp::Add, lhs: a3, rhs: one_ });
    // let mut acc = (0u32, 0u32); let mut i = 0;
    let z0 = b.int(0, SU32);
    let z1 = b.int(0, SU32);
    let acc_lit = b.tuple(&[z0, z1]);
    let zero = b.int(0, None);
    // while i < 4 { acc.0 += a[i]; acc.1 += 1; i += 1; }
    let acc_e = b.local(acc);
    let acc0 = b.tfield(acc_e, 0);
    let a_e = b.local(a);
    let i_e = b.local(i);
    let ai = b.index(a_e, i_e);
    let s1 = b.e(Expr::AssignOp { op: BinOp::Add, lhs: acc0, rhs: ai });
    let acc_e = b.local(acc);
    let acc1 = b.tfield(acc_e, 1);
    let one_ = b.int(1, None);
    let s2 = b.e(Expr::AssignOp { op: BinOp::Add, lhs: acc1, rhs: one_ });
    let i_e = b.local(i);
    let one_ = b.int(1, None);
    let s3 = b.e(Expr::AssignOp { op: BinOp::Add, lhs: i_e, rhs: one_ });
    let i_e = b.local(i);
    let four = b.int(4, None);
    let cond = b.bin(BinOp::Lt, i_e, four);
    let lp = b.while_(
        cond,
        &[Stmt::Expr { expr: s1, semi: true }, Stmt::Expr { expr: s2, semi: true }, Stmt::Expr { expr: s3, semi: true }],
    );
    // let (s, n) = acc; let [_, _, c, _] = a;
    let elems = b.pats_list(&[s, n]);
    let sn = b.pat(Pat::Tuple { elems, rest: None });
    let acc_e = b.local(acc);
    let w0 = b.pat(Pat::Wild);
    let w1 = b.pat(Pat::Wild);
    let w3 = b.pat(Pat::Wild);
    let elems = b.pats_list(&[w0, w1, c, w3]);
    let parts = b.pat(Pat::Slice { elems, rest: None });
    let a_e = b.local(a);
    // s * 100 + n * 10 + c
    let s_e = b.local(s);
    let hundred = b.int(100, None);
    let x = b.bin(BinOp::Mul, s_e, hundred);
    let n_e = b.local(n);
    let ten = b.int(10, None);
    let y = b.bin(BinOp::Mul, n_e, ten);
    let xy = b.bin(BinOp::Add, x, y);
    let c_e = b.local(c);
    let sum = b.bin(BinOp::Add, xy, c_e);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Expr { expr: set, semi: true },
            Stmt::Expr { expr: add5, semi: true },
            Stmt::Expr { expr: add1, semi: true },
            Stmt::Let { pat: acc, ty: None, init: Some(acc_lit), else_: None },
            Stmt::Let { pat: i, ty: None, init: Some(zero), else_: None },
            Stmt::Expr { expr: lp, semi: true },
            Stmt::Let { pat: sn, ty: None, init: Some(acc_e), else_: None },
            Stmt::Let { pat: parts, ty: None, init: Some(a_e), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 24 * 100 + 4 * 10 + 8);
}

/// `fn main(i: usize) -> u32 { let a = [7u32, 8, 9]; a[i] }`: in range the element; out of
/// range the run reaches the call of `panic_bounds_check`, which is the function's one ref.
#[test]
fn an_index_out_of_range_calls_the_bounds_fn() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[USIZE], U32);
    let mut b = BodyBuf::new();
    let i = b.bind("i");
    b.params.push(i);
    let a = b.bind("a");
    let e7 = b.int(7, SU32);
    let e8 = b.int(8, None);
    let e9 = b.int(9, None);
    let arr = b.array(&[e7, e8, e9]);
    let a_e = b.local(a);
    let i_e = b.local(i);
    let ai = b.index(a_e, i_e);
    let root = b.block(&[Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None }], Some(ai));
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("panic_bounds_check"), "no lang item fn is refused by name: {refused}");
    let bounds = w.bounds_fn();
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(built.refs.as_slice(), &[Instance { item: bounds, args: List::EMPTY }]);
    assert!(has_call_sig(&built, &[Ty::I64, Ty::I64], &[]));
    assert_eq!(one(&built, &[Val::Int(2)]), 9);
    assert_eq!(one(&built, &[Val::Int(0)]), 7);
    let refused = run(&built, &[Val::Int(3)]).expect_err("an index past the end runs the bounds fn");
    assert!(refused.contains("op: Call"), "{refused}");
    let text = blaze_lir::print(built.func.view());
    assert!(text.lines().any(|l| l.contains("call") && l.contains("ref0")), "{text}");
}

/// `let mut p = P { a: 1, b: 2 }; (&mut p).a = 7; *(&mut p.b) += 3; p.a + p.b as i32`, and
/// `fn set(r: &mut P, v: i32) { r.a = v }`, which stores through its pointer parameter.
#[test]
fn a_write_through_a_mutable_reference_is_seen() {
    let mut w = TestWorld::new();
    let p_ty = p_and_mk(&mut w);
    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let p = b.bind("p");
    let one_ = b.int(1, None);
    let two = b.int(2, None);
    let lit = p_lit(&mut b, one_, two);
    let p_e = b.local(p);
    let r = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: p_e });
    let ra = b.field(r, "a");
    let seven = b.int(7, None);
    let set = b.e(Expr::Assign { lhs: ra, rhs: seven });
    let p_e = b.local(p);
    let pb = b.field(p_e, "b");
    let r = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: pb });
    let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r });
    let three = b.int(3, None);
    let add = b.e(Expr::AssignOp { op: BinOp::Add, lhs: star, rhs: three });
    let p_e = b.local(p);
    let pa = b.field(p_e, "a");
    let p_e = b.local(p);
    let pb = b.field(p_e, "b");
    let i32_ty = b.types.intern(I32).expect("a type interns");
    let cast = b.e(Expr::Cast { operand: pb, ty: i32_ty });
    let sum = b.bin(BinOp::Add, pa, cast);
    let root = b.block(
        &[
            Stmt::Let { pat: p, ty: None, init: Some(lit), else_: None },
            Stmt::Expr { expr: set, semi: true },
            Stmt::Expr { expr: add, semi: true },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 7 + 5);

    let r_ty = w.ref_(Mutbl::Mut, p_ty);
    w.func(6, "set", &[r_ty, I32], UNIT);
    let mut callee = BodyBuf::new();
    let r = callee.bind("r");
    let v = callee.bind("v");
    callee.params.extend_from_slice(&[r, v]);
    let r_e = callee.local(r);
    let ra = callee.field(r_e, "a");
    let v_e = callee.local(v);
    let root = callee.e(Expr::Assign { lhs: ra, rhs: v_e });
    let mut g = PortableTys::new_in(Alloc::global());
    let set = build_instance(&w, &callee, ItemKey(6), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(set.func.view());
    assert_eq!(own_sig(&set), (vec_of(&[Ty::PTR, Ty::I32]), vec_of(&[])), "{text}");
    // `r` and `v` into their slots, then `v` through `r`.
    assert_eq!(text.lines().filter(|l| l.contains("store")).count(), 3, "{text}");
}

/// `fn stop(index: usize, len: usize) -> ! { loop {} }`: built, and it never returns.
#[test]
fn a_fn_returning_never_loops() {
    let mut w = TestWorld::new();
    w.bounds_fn();
    let mut b = BodyBuf::new();
    let i = b.bind("index");
    let n = b.bind("len");
    b.params.extend_from_slice(&[i, n]);
    let lp = b.e(Expr::Missing);
    let body = b.block(&[], None);
    b.set(lp, Expr::Loop { body });
    let root = b.block(&[], Some(lp));
    let mut g = PortableTys::new_in(Alloc::global());
    let built = build_instance(&w, &b, ItemKey(5), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(own_sig(&built), (vec_of(&[Ty::I64, Ty::I64]), vec_of(&[])));
    let ran = run(&built, &[Val::Int(1), Val::Int(0)]).expect_err("it never returns");
    assert!(ran.contains("out of fuel"), "{ran}");
}

/// The kinds and exts of signature `sig`'s parameters.
fn param_kinds(built: &Built<Global>, sig: SigId) -> Vec<(u8, u8), Global> {
    let view = built.func.view();
    let mut out = Vec::new_in(Global);
    out.extend(view.sig(sig).params.iter().map(|p| (p.kind, p.ext)));
    out
}

/// lib.rs, "The C ABI": `extern "C" fn mk(a: i32, b: u8) -> P`, `P` 8 bytes, returns P as
/// one `i64` read from its place; its `u8` is zero-extended (Apple). A call of it stores the
/// `i64` back at P's width.
#[test]
fn a_c_struct_of_8_bytes_is_returned_in_one_register() {
    let mut w = TestWorld::new();
    p_and_mk(&mut w);
    w.c_abi.push(ItemKey(3));
    let mut callee = BodyBuf::new();
    let a = callee.bind("a");
    let bb = callee.bind("b");
    callee.params.extend_from_slice(&[a, bb]);
    let a_e = callee.local(a);
    let b_e = callee.local(bb);
    let root = p_lit(&mut callee, a_e, b_e);
    let mut g = PortableTys::new_in(Alloc::global());
    let mk = build_instance(&w, &callee, ItemKey(3), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(mk.func.view());
    assert_eq!(own_sig(&mk), (vec_of(&[Ty::I32, Ty::I8]), vec_of(&[Ty::I64])), "{text}");
    let own = SigId(mk.func.view().head.sig);
    assert_eq!(param_kinds(&mk, own).as_slice(), &[(0, 0), (0, 1)], "the u8 is zext: {text}");
    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let x = b.int(1, None);
    let y = b.int(2, None);
    let call = b.call("mk", &[x, y]);
    let field = b.field(call, "a");
    let main = build(&w, &b, field).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(has_call_sig(&main, &[Ty::I32, Ty::I8], &[Ty::I64]), "{text}");
}

/// `#[repr(C)] struct Q { a: u32, b: u32, c: u32 }` (12 bytes) and `#[repr(C)] struct Big
/// { a: u64, b: u64, c: u64 }` (24): `extern "C" fn take(q: Q, big: Big) -> Big` takes Q in two
/// `i64`s, the last read as an `i32` inside its 12 bytes, Big by a `byval` pointer, and returns
/// Big through `sret`.
#[test]
fn c_composites_go_in_words_byval_and_sret() {
    let mut w = TestWorld::new();
    let u64_ = PNode::Uint(UintTy::U64);
    w.strukt(9, "Q", &[("a", U32), ("b", U32), ("c", U32)]);
    w.strukt(11, "Big", &[("a", u64_), ("b", u64_), ("c", u64_)]);
    let c = ReprAttrs { c: true, ..ReprAttrs::RUST };
    w.reprs.extend_from_slice(&[(ItemKey(9), c), (ItemKey(11), c)]);
    let (q, big) = (PNode::Adt(ItemKey(9), List::EMPTY), PNode::Adt(ItemKey(11), List::EMPTY));
    w.func(3, "take", &[q, big], big);
    w.c_abi.push(ItemKey(3));
    let mut callee = BodyBuf::new();
    let qp = callee.bind("q");
    let bp = callee.bind("big");
    callee.params.extend_from_slice(&[qp, bp]);
    let root = callee.local(bp);
    let mut g = PortableTys::new_in(Alloc::global());
    let take = build_instance(&w, &callee, ItemKey(3), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(take.func.view());
    assert_eq!(own_sig(&take), (vec_of(&[Ty::PTR, Ty::I64, Ty::I64, Ty::PTR]), vec_of(&[])), "{text}");
    let own = SigId(take.func.view().head.sig);
    let mut kinds = Vec::new_in(Global);
    kinds.extend(param_kinds(&take, own).iter().map(|&(k, _)| k));
    let (sret, byval) = (blaze_lir::func::ParamKind::Sret as u8, blaze_lir::func::ParamKind::Byval as u8);
    assert_eq!(kinds.as_slice(), &[sret, 0, 0, byval], "{text}");
    // Q's second word: its bytes 8..12 as one `i32` store, never an `i64` past its end.
    // Q's slot (`v4`) takes two stores: the first word whole, the second truncated to `i32`.
    assert!(text.contains("v6 = trunc i32 v2") && text.contains("store v6, v4, 8"), "{text}");
    assert_eq!(text.lines().filter(|l| l.contains("store") && l.contains(", v4, ")).count(), 2, "{text}");
}

/// An instance's key is its code identity and its arguments as trees: two ground tables that
/// hold the same arguments at different indices give one key; other arguments, or other code,
/// another.
#[test]
fn instance_keys_follow_the_arguments_not_the_table() {
    use blaze_typeck::{ItemKey, PNode, PortableTys, UintTy, IntTy};
    let pair = ItemKey(7);
    let code = blaze_ids::ContentId::of("test code", b"fn second");
    let mut first = PortableTys::new_in(Alloc::global());
    let u8_ = first.intern(PNode::Uint(UintTy::U8)).expect("intern");
    let inner = first.intern_list(&[u8_]).expect("intern");
    let adt = first.intern(PNode::Adt(pair, inner)).expect("intern");
    let a = first.intern_list(&[adt, u8_]).expect("intern");
    let mut second = PortableTys::new_in(Alloc::global());
    let _unrelated = second.intern(PNode::Int(IntTy::I64)).expect("intern");
    let u8_2 = second.intern(PNode::Uint(UintTy::U8)).expect("intern");
    let inner2 = second.intern_list(&[u8_2]).expect("intern");
    let adt2 = second.intern(PNode::Adt(pair, inner2)).expect("intern");
    let b = second.intern_list(&[adt2, u8_2]).expect("intern");
    let other = second.intern_list(&[u8_2, adt2]).expect("intern");
    let key = |table: &PortableTys<'_>, args, code| crate::instance_key(code, args, table.view(), Alloc::global()).expect("a key");
    assert_eq!(key(&first, a, code), key(&second, b, code));
    assert_ne!(key(&second, b, code), key(&second, other, code));
    assert_ne!(key(&first, a, code), key(&first, a, blaze_ids::ContentId::of("test code", b"fn second2")));
}

const DROP_IN_PLACE: ItemKey = ItemKey(20);
const G_DROP: ItemKey = ItemKey(21);
const H_DROP: ItemKey = ItemKey(22);

/// `struct G { x: i32 }` with a `Drop` impl (item 9), `fn consume(g: G)` (item 7) and
/// `#[lang = "drop_in_place"] fn drop_in_place<T>(p: *mut T)`: the type `G`.
fn g_world(w: &mut TestWorld) -> PNode {
    w.strukt(9, "G", &[("x", I32)]);
    let g = PNode::Adt(ItemKey(9), List::EMPTY);
    w.drops.push((ItemKey(9), G_DROP));
    w.func(7, "consume", &[g], UNIT);
    let to = w.ty(T);
    w.generic_func(20, "drop_in_place", 1, &[PNode::Ptr(Mutbl::Mut, to)], UNIT);
    w.drop_in_place = Some(DROP_IN_PLACE);
    g
}

/// `G { x: v }`.
fn g_lit(b: &mut BodyBuf, v: u128) -> ExprId {
    let path = b.path("G");
    let x = b.int(v, None);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("x"), expr: x });
    b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None })
}

/// How many calls the function makes of its `n`th reference.
fn calls_of(built: &Built<Global>, n: usize) -> usize {
    let text = blaze_lir::print(built.func.view());
    let name = format!("ref{n}");
    text.lines().filter(|l| l.contains("call") && l.split(|c: char| !c.is_alphanumeric()).any(|w| w == name)).count()
}

/// `fn main(c: bool) -> i32 { let a = G {..}; let b = G {..}; if c { consume(a); } let d = b; 7 }`:
/// `a` is moved on one path only, so it gets a flag and one guarded drop; `b` is moved
/// unconditionally and never dropped; `d` is dropped once at the block's end (the reference's
/// "Destructors": scopes, moves and drop flags).
#[test]
fn a_value_moved_on_one_path_is_dropped_by_its_flag() {
    let mut w = TestWorld::new();
    g_world(&mut w);
    w.func(1, "main", &[PNode::Bool], I32);
    let mut b = BodyBuf::new();
    let c = b.bind("c");
    b.params.push(c);
    let a = b.bind("a");
    let bb = b.bind("b");
    let d = b.bind("d");
    let a_init = g_lit(&mut b, 1);
    let b_init = g_lit(&mut b, 2);
    let a_e = b.local(a);
    let call = b.call("consume", &[a_e]);
    let then = b.block(&[Stmt::Expr { expr: call, semi: true }], None);
    let cond = b.local(c);
    let iff = b.e(Expr::If { cond, then, else_: None });
    let b_e = b.local(bb);
    let seven = b.int(7, None);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: bb, ty: None, init: Some(b_init), else_: None },
            Stmt::Expr { expr: iff, semi: false },
            Stmt::Let { pat: d, ty: None, init: Some(b_e), else_: None },
        ],
        Some(seven),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    let glue = built.refs.iter().position(|r| r.item == DROP_IN_PLACE).unwrap_or_else(|| panic!("no drop glue is called:\n{text}"));
    assert_eq!(calls_of(&built, glue), 2, "a's guarded drop and d's, never b's:\n{text}");
    let flags = built.func.view().slots.iter().filter(|s| s.size == 1).count();
    assert_eq!(flags, 2, "`c`'s slot and one flag, a's:\n{text}");

    // Without the conditional move, no flag: every state is known statically.
    let mut b2 = BodyBuf::new();
    let a2 = b2.bind("a");
    let c2 = b2.bind("c");
    b2.params.push(c2);
    let init = g_lit(&mut b2, 1);
    let seven = b2.int(7, None);
    let root = b2.block(&[Stmt::Let { pat: a2, ty: None, init: Some(init), else_: None }], Some(seven));
    let built = build(&w, &b2, root).unwrap_or_else(|e| panic!("{e}"));
    let glue = built.refs.iter().position(|r| r.item == DROP_IN_PLACE).expect("a's drop");
    assert_eq!(calls_of(&built, glue), 1);
    assert_eq!(built.func.view().slots.iter().filter(|s| s.size == 1).count(), 1, "`c`'s slot only");
}

/// The glue of `struct H { g: G, n: i32, g2: G }` with its own `Drop`: `<H as Drop>::drop`
/// first, then each field that needs drop in declaration order (the reference's
/// "Destructors": the value's own destructor, then its fields).
#[test]
fn drop_glue_runs_the_impl_then_the_fields_in_order() {
    let mut w = TestWorld::new();
    let g = g_world(&mut w);
    w.strukt(10, "H", &[("g", g), ("n", I32), ("g2", g)]);
    w.drops.push((ItemKey(10), H_DROP));
    let mut b = BodyBuf::new();
    let p = b.bind("to_drop");
    b.params.push(p);
    let root = b.block(&[], None);
    let view = b.view(DROP_IN_PLACE, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let mut ground = PortableTys::new_in(Alloc::global());
    let h = ground.intern(PNode::Adt(ItemKey(10), List::EMPTY)).expect("intern");
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let mut subst = Subst { args: &[h], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
        .unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    let g_ty = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("intern");
    let h_list = ground.intern_list(&[h]).expect("intern");
    let g_list = ground.intern_list(&[g_ty]).expect("intern");
    assert_eq!(
        built.refs.as_slice(),
        &[Instance { item: H_DROP, args: h_list }, Instance { item: DROP_IN_PLACE, args: g_list }],
        "{text}"
    );
    assert_eq!(calls_of(&built, 0), 1, "{text}");
    assert_eq!(calls_of(&built, 1), 2, "g, then g2:\n{text}");
    // The calls in order: the impl's, then g's at offset 0, then g2's at offset 8.
    let order: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(text.lines().filter(|l| l.contains("call")));
        v
    };
    assert!(order[0].contains("ref0"), "{text}");
}

/// `enum E { A, B(i32, u8), C(u8) }` (key 30; variants 31, 32, 33).
fn e_world(w: &mut TestWorld) -> PNode {
    w.enum_(
        30,
        &[("A", CtorKind::Unit, &[]), ("B", CtorKind::Tuple, &[("0", I32), ("1", U8)]), ("C", CtorKind::Tuple, &[("0", U8)])],
    );
    PNode::Adt(ItemKey(30), List::EMPTY)
}

/// lib.rs, "Enums": `fn main(k: i32) -> i32 { let e = match k { 0 => A, 1 => B(40, 2), _ => C(9) };
/// let f = e; match f { A => 1, B(a, b) => a + b as i32, C(c) => c as i32 + 100 } }`: each
/// variant built (tag, then its fields), copied by its tag, and taken apart by a switch on it.
#[test]
fn an_enum_is_built_copied_and_matched() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[I32], I32);
    e_world(&mut w);
    let mut b = BodyBuf::new();
    let k = b.bind("k");
    b.params.push(k);
    let e = b.bind("e");
    let f = b.bind("f");
    // match k { 0 => A, 1 => B(40, 2), _ => C(9) }
    let p0 = b.pat_int(0);
    let p1 = b.pat_int(1);
    let wild = b.pat(Pat::Wild);
    let a_path = b.path("A");
    let a_val = b.e(Expr::Path(a_path));
    let forty = b.int(40, None);
    let two = b.int(2, None);
    let b_val = b.call("B", &[forty, two]);
    let nine = b.int(9, None);
    let c_val = b.call("C", &[nine]);
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: p0, guard: None, body: a_val });
    b.arms.push(Arm { pat: p1, guard: None, body: b_val });
    b.arms.push(Arm { pat: wild, guard: None, body: c_val });
    let k_e = b.local(k);
    let make = b.e(Expr::Match { scrutinee: k_e, arms: List { start, len: 3 } });
    // match f { A => 1, B(a, b) => a + b as i32, C(c) => c as i32 + 100 }
    let pa_path = b.path("A");
    let pa = b.pat(Pat::Path(pa_path));
    let x = b.bind("a");
    let y = b.bind("b");
    let pb_path = b.path("B");
    let elems = b.pats_list(&[x, y]);
    let pb = b.pat(Pat::TupleStruct { path: pb_path, elems, rest: None });
    let z = b.bind("c");
    let pc_path = b.path("C");
    let elems = b.pats_list(&[z]);
    let pc = b.pat(Pat::TupleStruct { path: pc_path, elems, rest: None });
    let i32_ty = b.types.intern(I32).expect("a type interns");
    let one_ = b.int(1, None);
    let x_e = b.local(x);
    let y_e = b.local(y);
    let y_i = b.e(Expr::Cast { operand: y_e, ty: i32_ty });
    let sum = b.bin(BinOp::Add, x_e, y_i);
    let z_e = b.local(z);
    let z_i = b.e(Expr::Cast { operand: z_e, ty: i32_ty });
    let hundred = b.int(100, None);
    let plus = b.bin(BinOp::Add, z_i, hundred);
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: pa, guard: None, body: one_ });
    b.arms.push(Arm { pat: pb, guard: None, body: sum });
    b.arms.push(Arm { pat: pc, guard: None, body: plus });
    let f_e = b.local(f);
    let take = b.e(Expr::Match { scrutinee: f_e, arms: List { start, len: 3 } });
    let e_e = b.local(e);
    let root = b.block(
        &[Stmt::Let { pat: e, ty: None, init: Some(make), else_: None }, Stmt::Let { pat: f, ty: None, init: Some(e_e), else_: None }],
        Some(take),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[Val::Int(0)]), 1);
    assert_eq!(one(&built, &[Val::Int(1)]), 42);
    assert_eq!(one(&built, &[Val::Int(2)]), 109);
}

/// lib.rs, "Intrinsics": `transmute::<(u32, u32), [u32; 2]>((40, 2))` read back as an array,
/// and `transmute::<u32, i32>(7)` keeping the bits.
#[test]
fn transmute_keeps_the_bytes() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.bounds_fn();
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let mut b = BodyBuf::new();
    let a = b.bind("a");
    let forty = b.int(40, SU32);
    let two = b.int(2, SU32);
    let pair = b.tuple(&[forty, two]);
    let call = b.call("transmute", &[pair]);
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let arr_ty = b.types.array_n(u32_ty, 2).expect("a type interns");
    let a0 = b.local(a);
    let i0 = b.int(0, None);
    let x0 = b.index(a0, i0);
    let a1 = b.local(a);
    let i1 = b.int(1, None);
    let x1 = b.index(a1, i1);
    let sum = b.bin(BinOp::Add, x0, x1);
    let root = b.block(&[Stmt::Let { pat: a, ty: Some(arr_ty), init: Some(call), else_: None }], Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 42);
}

/// core's `ub_checks()` reached through the intrinsic table (core ub_checks.rs
/// `check_language_ub`'s shape, `.. && intrinsics::ub_checks()` guarding a precondition):
/// ```text
/// fn main(x: u32) -> u32 { if ub_checks() { if x < 10 { 1 } else { 2 } } else { 3 } }
/// ```
/// The answer is the building profile's (`Adts::ub_checks`): with the checks on the
/// precondition is checked (1 for 5, 2 for 12), with them off it is not (3). And
/// `overflow_checks()` answers the overflow setting the same way. A builder that ignored the
/// setting answers one of the two settings wrongly.
/// Prints "ub_checks: on 1 2, off 3 3; overflow_checks: on 1, off 0".
#[test]
fn ub_checks_is_the_building_profiles_answer() {
    let body = |b: &mut BodyBuf| {
        let x = b.bind("x");
        b.params.push(x);
        let asked = b.call("ub_checks", &[]);
        let x_e = b.local(x);
        let ten = b.int(10, SU32);
        let cond = b.bin(BinOp::Lt, x_e, ten);
        let one_ = b.int(1, SU32);
        let then_in = b.block(&[], Some(one_));
        let two = b.int(2, SU32);
        let else_in = b.block(&[], Some(two));
        let inner = b.e(Expr::If { cond, then: then_in, else_: Some(else_in) });
        let then = b.block(&[], Some(inner));
        let three = b.int(3, SU32);
        let else_ = b.block(&[], Some(three));
        b.e(Expr::If { cond: asked, then, else_: Some(else_) })
    };
    let mut seen = std::vec::Vec::new();
    for ub in [crate::UbChecks::On, crate::UbChecks::Off] {
        let mut w = TestWorld::new();
        w.ub = ub;
        w.func(1, "main", &[U32], U32);
        w.func(6, "ub_checks", &[], PNode::Bool);
        w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::UbChecks)));
        let mut b = BodyBuf::new();
        let root = body(&mut b);
        let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
        seen.push((one(&built, &[Val::Int(5)]), one(&built, &[Val::Int(12)])));
    }
    assert_eq!(seen, [(1, 2), (3, 3)], "ub_checks() under On, then Off");
    let mut flows = std::vec::Vec::new();
    for overflow in [crate::OverflowChecks::On, crate::OverflowChecks::Off] {
        let mut w = TestWorld::new();
        w.overflow = overflow;
        w.func(1, "main", &[], PNode::Bool);
        w.func(6, "overflow_checks", &[], PNode::Bool);
        w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::OverflowChecks)));
        let mut b = BodyBuf::new();
        let root = b.call("overflow_checks", &[]);
        let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
        flows.push(one(&built, &[]));
    }
    assert_eq!(flows, [1, 0], "overflow_checks() under On, then Off");
    std::println!("ub_checks: on {} {}, off {} {}; overflow_checks: on {}, off {}", seen[0].0, seen[0].1, seen[1].0, seen[1].1, flows[0], flows[1]);
}

/// `panic_bounds_check` on the host: an index out of bounds stops the test.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_bounds(index: u64, len: u64) {
    panic!("an index out of bounds in emitted code: {index} of {len}");
}

/// Every `panic_const_*` entry point ([`PANIC_CONST`]) on the host: a failed arithmetic check
/// in emitted code stops the test.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_panic_const() {
    panic!("an arithmetic check failed in emitted code");
}

/// `const_eval_select`'s run-time fn on the host: `a * 10 + b`.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_rt(a: u32, b: u32) -> u32 {
    a * 10 + b
}

/// The four pointer intrinsics on the hosted hello's path (`is_null`, `ptr::null`, slice
/// lengths), run:
/// ```text
/// let arr = [1u8, 2, 3]; let s: &[u8] = &arr;
/// let n: usize = ptr_metadata(s);                 // 3
/// let w: &[u8] = aggregate_raw_ptr(&arr, 2usize);  // (arr's address, 2)
/// let m: usize = ptr_metadata(w);                 // 2
/// let x = 7u32; let y = 7u32;
/// let e1: u8 = ptr_guaranteed_cmp(&x, &x);        // 1
/// let e2: u8 = ptr_guaranteed_cmp(&x, &y);        // 0
/// let r: u32 = const_eval_select((4u32, 2u32), cf, rt);   // rt(4, 2) = 42
/// n * 10000 + m * 1000 + e1 * 100 + e2 * 10 + w[1] + r     // 32144 (w[1] through the
/// ```                                                          // slice's own length check)
#[test]
fn pointer_intrinsics_run() {
    use crate::intrinsics::Which as W;
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    let p = |i| PNode::Param(i);
    w.generic_func(40, "ptr_metadata", 2, &[p(0)], p(1));
    w.generic_func(41, "aggregate_raw_ptr", 3, &[p(1), p(2)], p(0));
    w.generic_func(42, "ptr_guaranteed_cmp", 1, &[p(0), p(0)], U8);
    w.generic_func(43, "const_eval_select", 4, &[p(0), p(1), p(2)], p(3));
    w.func(44, "cf", &[U32, U32], U32);
    w.func(45, "rt", &[U32, U32], U32);
    for (k, which) in [(40, W::PtrMetadata), (41, W::AggregateRawPtr), (42, W::PtrGuaranteedCmp), (43, W::ConstEvalSelect)] {
        w.intrinsics.push((ItemKey(k), Intrinsic::Table(which)));
    }
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let slice_ref = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let (arr, s, n, wl, m, x, y, e1, e2, r) =
        (b.bind("arr"), b.bind("s"), b.bind("n"), b.bind("w"), b.bind("m"), b.bind("x"), b.bind("y"), b.bind("e1"), b.bind("e2"), b.bind("r"));
    let su8 = Some(IntSuffix::Uint(UintTy::U8));
    let (one_, two_, three_) = (b.int(1, su8), b.int(2, su8), b.int(3, su8));
    let elems = b.exprs_list(&[one_, two_, three_]);
    let arr_init = b.e(Expr::Array { elems });
    let arr_e = b.local(arr);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr_e });
    let s_e = b.local(s);
    let n_init = b.call("ptr_metadata", &[s_e]);
    let arr_e = b.local(arr);
    let arr_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr_e });
    let two_us = b.int(2, Some(IntSuffix::Uint(UintTy::Usize)));
    let w_init = b.call("aggregate_raw_ptr", &[arr_ref, two_us]);
    let w_e = b.local(wl);
    let m_init = b.call("ptr_metadata", &[w_e]);
    let (x_init, y_init) = (b.int(7, SU32), b.int(7, SU32));
    let addr = |b: &mut BodyBuf, l| {
        let e = b.local(l);
        b.e(Expr::Ref { mutbl: Mutbl::Not, operand: e })
    };
    let (ax, ax2) = (addr(&mut b, x), addr(&mut b, x));
    let e1_init = b.call("ptr_guaranteed_cmp", &[ax, ax2]);
    let (ax3, ay) = (addr(&mut b, x), addr(&mut b, y));
    let e2_init = b.call("ptr_guaranteed_cmp", &[ax3, ay]);
    let (four, two) = (b.int(4, SU32), b.int(2, SU32));
    let pair = b.tuple(&[four, two]);
    let cf_path = b.path("cf");
    let cf = b.e(Expr::Path(cf_path));
    let rt_path = b.path("rt");
    let rt = b.e(Expr::Path(rt_path));
    let r_init = b.call("const_eval_select", &[pair, cf, rt]);
    // n * 10000 + m * 1000 + e1 * 100 + e2 * 10 + w[1] + r
    let term = |b: &mut BodyBuf, l, k: u128| {
        let e = b.local(l);
        let c = b.e(Expr::Cast { operand: e, ty: usize_ty });
        let k = b.int(k, None);
        b.bin(BinOp::Mul, c, k)
    };
    let tn = term(&mut b, n, 10000);
    let tm = term(&mut b, m, 1000);
    let t1 = term(&mut b, e1, 100);
    let t2 = term(&mut b, e2, 10);
    let tr = term(&mut b, r, 1);
    let w_e = b.local(wl);
    let i1 = b.int(1, None);
    let w1 = b.index(w_e, i1);
    let tw = b.e(Expr::Cast { operand: w1, ty: usize_ty });
    let mut sum = b.bin(BinOp::Add, tn, tm);
    for t in [t1, t2, tw, tr] {
        sum = b.bin(BinOp::Add, sum, t);
    }
    let let_ = |pat, ty, init| Stmt::Let { pat, ty, init: Some(init), else_: None };
    let root = b.block(
        &[
            let_(arr, None, arr_init),
            let_(s, Some(slice_ref), s_init),
            let_(n, Some(usize_ty), n_init),
            let_(wl, Some(slice_ref), w_init),
            let_(m, Some(usize_ty), m_init),
            let_(x, None, x_init),
            let_(y, None, y_init),
            let_(e1, Some(u8_ty), e1_init),
            let_(e2, Some(u8_ty), e2_init),
            let_(r, Some(u32_ty), r_init),
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    // The run-time fn is called directly; the const fn is never referenced.
    assert!(built.refs.iter().any(|r| r.item == ItemKey(45)), "{text}");
    assert!(!built.refs.iter().any(|r| r.item == ItemKey(44)), "{text}");
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(45), host_rt as usize as u64), (ItemKey(5), host_bounds as usize as u64)];
        let got = run_native_with(&host, &[(MAIN, None, &built)]);
        assert_eq!(got, 30000 + 2000 + 100 + 0 + 2 + 42, "{text}");
    }
}

/// lib.rs, "Wide pointers": `let s: &str = "hello";` is the bytes as data, then their address
/// at offset 0 and their length, 5, at offset 8 of a 16-byte pair.
#[test]
fn a_string_literal_is_data_and_a_length() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let mut b = BodyBuf::new();
    let s = b.bind("s");
    let lit = b.e(Expr::Lit(Lit::Str));
    b.strs.push((lit, "hello"));
    let root = b.block(&[Stmt::Let { pat: s, ty: None, init: Some(lit), else_: None }], None);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("bytes \"68656c6c6f\""), "{text}");
    assert!(text.contains("data_addr"), "{text}");
    assert!(text.lines().any(|l| l.contains("iconst") && l.ends_with(" 5")), "{text}");
}

/// lib.rs, "Statics": `static mut N: u32` (item 20) and `#[thread_local] static T: u32`
/// (item 21); `fn main() -> u32 { N = N + T; N }` reaches N by `data_addr` of a `Static`
/// reference and T by `tls_addr` of a `Tls` one, each listed once in `Built`.
#[test]
fn a_static_and_a_thread_local_are_places() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(20, "N", &[], U32);
    w.func(21, "T", &[], U32);
    w.statics.push((ItemKey(20), StaticItem { thread_local: false, mutable: true }));
    w.statics.push((ItemKey(21), StaticItem { thread_local: true, mutable: false }));
    let mut b = BodyBuf::new();
    let n_path = b.path("N");
    let n_lhs = b.e(Expr::Path(n_path));
    let n_path = b.path("N");
    let n_read = b.e(Expr::Path(n_path));
    let t_path = b.path("T");
    let t_read = b.e(Expr::Path(t_path));
    let sum = b.bin(BinOp::Add, n_read, t_read);
    let assign = b.e(Expr::Assign { lhs: n_lhs, rhs: sum });
    let n_path = b.path("N");
    let n_tail = b.e(Expr::Path(n_path));
    let root = b.block(&[Stmt::Expr { expr: assign, semi: true }], Some(n_tail));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("data_addr"), "{text}");
    assert!(text.contains("tls_addr"), "{text}");
    assert_eq!(built.statics.as_slice(), &[ItemKey(20)]);
    assert_eq!(built.thread_locals.as_slice(), &[ItemKey(21)]);
    // N is written once, through its address.
    assert_eq!(text.lines().filter(|l| l.contains("store")).count(), 1, "{text}");
}

/// The record of one allocation of `bytes`, aligned to `2^align`, with no relocation.
fn record(bytes: &[u8], align: u8) -> DataRecord<Global> {
    let mut b = Vec::new_in(Global);
    b.extend_from_slice(bytes);
    let mut allocs = Vec::new_in(Global);
    allocs.push(DataAlloc { bytes: b, align, mutable: false, relocs: Vec::new_in(Global) });
    DataRecord { allocs }
}

/// lib.rs, "Consts": `const OK: u32 = 7` is a constant of its bytes; `fn main() -> u32 {
/// OK + 1 }` runs to 8. `const PAIR: (u8, u32) = (3, 9)`, laid out u32 first (repr(Rust)
/// orders by alignment: bytes 9 0 0 0 3 0 0 0), is a data row copied into a temporary;
/// `PAIR.0 as u32 + PAIR.1` is built from it. A const with no value is refused by name.
#[test]
fn consts_are_built_from_their_records() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(30, "OK", &[], U32);
    w.consts.push((ItemKey(30), Some(record(&[7, 0, 0, 0], 2))));
    let mut b = BodyBuf::new();
    let ok = b.path("OK");
    let ok = b.e(Expr::Path(ok));
    let one_ = b.int(1, None);
    let root = b.bin(BinOp::Add, ok, one_);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[]), 8);

    let pair = w.tuple(&[U8, U32]);
    w.func(31, "PAIR", &[], pair);
    w.consts.push((ItemKey(31), Some(record(&[9, 0, 0, 0, 3, 0, 0, 0], 2))));
    let mut b = BodyBuf::new();
    let p0 = b.path("PAIR");
    let p0 = b.e(Expr::Path(p0));
    let x0 = b.tfield(p0, 0);
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let x0 = b.e(Expr::Cast { operand: x0, ty: u32_ty });
    let p1 = b.path("PAIR");
    let p1 = b.e(Expr::Path(p1));
    let x1 = b.tfield(p1, 1);
    let root = b.bin(BinOp::Add, x0, x1);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("bytes \"0900000003000000\""), "{text}");
    assert!(text.contains("data_addr"), "{text}");

    w.consts[0].1 = None;
    let mut b = BodyBuf::new();
    let ok = b.path("OK");
    let root = b.e(Expr::Path(ok));
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("const item 0x1e"), "{refused}");
}

/// lib.rs, "Fn pointers": `#[repr(C)] struct Answer { code: u32, value: u64 }` (16 bytes) and
/// `struct Table { write: extern "C" fn(*mut u8, u32, usize, usize) -> Answer }`; `fn main(table:
/// Table, host: *mut u8, n: usize) -> u32 { (table.write)(host, 1, 0, n).code }` is a
/// `call_indirect` of the C signature, its result two `i64`s stored at 0 and 8.
#[test]
fn a_c_fn_pointer_returns_16_bytes_in_two_registers() {
    let mut w = TestWorld::new();
    let u64_ = PNode::Uint(UintTy::U64);
    w.strukt(9, "Answer", &[("code", U32), ("value", u64_)]);
    w.reprs.push((ItemKey(9), ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let answer = PNode::Adt(ItemKey(9), List::EMPTY);
    let u8_ = w.ty(U8);
    let ptr = PNode::Ptr(Mutbl::Mut, u8_);
    let mut sig = Vec::new_in(Global);
    for n in [ptr, U32, USIZE, USIZE, answer] {
        sig.push(w.ty(n));
    }
    let list = w.types.intern_list(&sig).expect("a list interns");
    let header = blaze_typeck::FnPtrHeader { abi: blaze_typeck::Abi::C, safety: blaze_typeck::Safety::Safe, c_variadic: false };
    let write = PNode::FnPtr(list, header);
    w.strukt(11, "Table", &[("write", write)]);
    let table = PNode::Adt(ItemKey(11), List::EMPTY);
    w.func(1, "main", &[table, ptr, USIZE], U32);
    let mut b = BodyBuf::new();
    let t = b.bind("table");
    let h = b.bind("host");
    let n = b.bind("n");
    b.params.extend_from_slice(&[t, h, n]);
    let t_e = b.local(t);
    let f = b.field(t_e, "write");
    let callee = b.e(Expr::Paren(f));
    let h_e = b.local(h);
    let one_ = b.int(1, None);
    let zero = b.int(0, None);
    let n_e = b.local(n);
    let args = b.exprs_list(&[h_e, one_, zero, n_e]);
    let call = b.e(Expr::Call { callee, args });
    let root = b.field(call, "code");
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("call_indirect"), "{text}");
    assert!(has_call_sig(&built, &[Ty::PTR, Ty::I32, Ty::I64, Ty::I64], &[Ty::I64, Ty::I64]), "{text}");
    assert_eq!(text.lines().filter(|l| l.contains("project")).count(), 2, "{text}");
}

/// lib.rs, "Fn pointers": `fn id(x: u32) -> u32` reified, `let f: fn(u32) -> u32 = id; f(5)`:
/// `func_addr` of id's reference, then a `call_indirect` of the Rust signature.
#[test]
fn a_fn_item_reified_is_called_through_its_address() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(3, "id", &[U32], U32);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let fl = b.types.intern_list(&[u32_ty, u32_ty]).expect("a list interns");
    let fn_ty = b.types.intern(PNode::FnPtr(fl, blaze_typeck::FnPtrHeader::RUST)).expect("a type interns");
    let f = b.bind("f");
    let id_path = b.path("id");
    let id = b.e(Expr::Path(id_path));
    let f_e = b.local(f);
    let five = b.int(5, None);
    let args = b.exprs_list(&[five]);
    let call = b.e(Expr::Call { callee: f_e, args });
    let root = b.block(&[Stmt::Let { pat: f, ty: Some(fn_ty), init: Some(id), else_: None }], Some(call));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("func_addr"), "{text}");
    assert!(text.contains("call_indirect"), "{text}");
    assert!(has_call_sig(&built, &[Ty::I32], &[Ty::I32]), "{text}");
    // Run for real: `fn id(x: u32) -> u32 { x }` placed first, then main through the pointer.
    let mut callee = BodyBuf::new();
    let x = callee.bind("x");
    callee.params.push(x);
    let x_e = callee.local(x);
    let root = callee.block(&[], Some(x_e));
    let mut g = PortableTys::new_in(Alloc::global());
    let id = build_instance(&w, &callee, ItemKey(3), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(ItemKey(3), &id), (MAIN, &built)]) as u32, 5);
}

/// `extern "rust-call" fn f(a: u32, t: (u32, u32, (u32, u32))) -> u32 { t.2.1 * 1000 + t.1 *
/// 100 + t.0 * 10 + a }`, `extern "rust-call" fn g(a: u32, t: ()) -> u32 { a }`, and `main` =
/// `f(1, (2, 3, (4, 5))) + g(7, ()) * 10000`: 75321.
///
/// The rule: core's library/core/src/ops/function.rs declares `extern "rust-call" fn
/// call_once(self, args: Args) -> Self::Output`; the convention of "rust-call" is the
/// compiler's own (not a stable ABI), and blaze's (codegen.md, "Closures and the Fn traits")
/// passes the last tuple's fields as separate parameters after the others, so a call through
/// `Fn*` (its arguments written apart) and a call with the tuple written whole agree. Both the
/// callee and the caller spread: `f`'s own signature is five registers (the inner pair by
/// address), `g`'s one (the empty tuple is nothing).
#[test]
fn a_rust_call_fn_takes_its_tuples_fields_as_parameters() {
    let mut w = TestWorld::new();
    let inner = w.tuple(&[U32, U32]);
    let t = w.tuple(&[U32, U32, inner]);
    let unit = w.tuple(&[]);
    w.func(1, "main", &[], U32);
    w.func(3, "f", &[U32, t], U32);
    w.func(4, "g", &[U32, unit], U32);
    w.rust_call.extend([ItemKey(3), ItemKey(4)]);
    let mut f = BodyBuf::new();
    let a = f.bind("a");
    let tp = f.bind("t");
    f.params.extend([a, tp]);
    let term = |f: &mut BodyBuf, path: &[u32], scale: u128| {
        let mut x = f.local(tp);
        for &i in path {
            x = f.tfield(x, i);
        }
        let k = f.int(scale, None);
        f.bin(BinOp::Mul, x, k)
    };
    let t21 = term(&mut f, &[2, 1], 1000);
    let t1 = term(&mut f, &[1], 100);
    let t0 = term(&mut f, &[0], 10);
    let a_e = f.local(a);
    let s = f.bin(BinOp::Add, t21, t1);
    let s = f.bin(BinOp::Add, s, t0);
    let s = f.bin(BinOp::Add, s, a_e);
    let root = f.block(&[], Some(s));
    let mut ground = PortableTys::new_in(Alloc::global());
    let fb = build_instance(&w, &f, ItemKey(3), root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(fb.func.view());
    assert_eq!(own_sig(&fb), (vec_of(&[Ty::I32, Ty::I32, Ty::I32, Ty::PTR]), vec_of(&[Ty::I32])), "{text}");

    let mut g = BodyBuf::new();
    let a = g.bind("a");
    let up = g.bind("t");
    g.params.extend([a, up]);
    let a_e = g.local(a);
    let root = g.block(&[], Some(a_e));
    let gb = build_instance(&w, &g, ItemKey(4), root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(gb.func.view());
    assert_eq!(own_sig(&gb), (vec_of(&[Ty::I32]), vec_of(&[Ty::I32])), "{text}");

    let mut b = BodyBuf::new();
    let n: [ExprId; 5] = core::array::from_fn(|i| b.int(i as u128 + 1, None));
    let inner_e = b.tuple(&[n[3], n[4]]);
    let tuple_e = b.tuple(&[n[1], n[2], inner_e]);
    let call_f = b.call("f", &[n[0], tuple_e]);
    let seven = b.int(7, None);
    let unit_e = b.tuple(&[]);
    let call_g = b.call("g", &[seven, unit_e]);
    let k = b.int(10000, None);
    let scaled = b.bin(BinOp::Mul, call_g, k);
    let sum = b.bin(BinOp::Add, call_f, scaled);
    let root = b.block(&[], Some(sum));
    let main = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(has_call_sig(&main, &[Ty::I32, Ty::I32, Ty::I32, Ty::PTR], &[Ty::I32]), "the caller spreads too\n{text}");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(ItemKey(3), &fb), (ItemKey(4), &gb), (MAIN, &main)]) as u32, 75321, "{text}");
}

/// Emits each built fn (`Via::Address`), maps it, and calls the last: a native run on this
/// machine. `fns` lists callees before their callers; a call names its callee by item (a
/// trait method's instance stands for its one impl here, as placement resolves it).
#[cfg(target_arch = "aarch64")]
fn run_native(fns: &[(ItemKey, &Built<Global>)]) -> u64 {
    let mut all = Vec::new_in(Global);
    all.extend(fns.iter().map(|&(k, b)| (k, None, b)));
    run_native_with(&[], &all)
}

/// `run_native` with host functions (`extern "C"` fns of this test binary, by item) and fns
/// named by instance: an entry with arguments is the callee only of a reference at exactly
/// those arguments (one ground table for all), one with `None` of any.
#[cfg(target_arch = "aarch64")]
fn run_native_with(host: &[(ItemKey, u64)], fns: &[(ItemKey, Option<List>, &Built<Global>)]) -> u64 {
    run_native_vt(host, fns, None)
}

/// What the native run needs to place the vtables a built fn names (`Built::vtables`): the
/// trait facts, the impl fn of each (trait method, concrete type), and each concrete type's
/// size, alignment and drop glue (a placed fn's item). Tables are placed once, as the program's
/// placement does, their method words the placed impl fns' entries.
#[cfg(target_arch = "aarch64")]
struct Tables<'a> {
    facts: &'a dyn Fn(ItemKey) -> Option<crate::vtable::ObjectTrait<'a>>,
    impls: &'a [(ItemKey, PTy, ItemKey)],
    sizes: &'a [(PTy, u64, u64, Option<ItemKey>)],
    ground: &'a mut PortableTys<'static>,
    placed: Vec<(crate::vtable::VtableRef, u64), Global>,
}

#[cfg(target_arch = "aarch64")]
impl Tables<'_> {
    /// The address of `want`'s table, placed (leaked) on first use; the tables of supertraits
    /// off the first chain first, by a stack.
    fn place(&mut self, want: crate::vtable::VtableRef, fn_addr: &dyn Fn(ItemKey, Option<List>) -> u64) -> u64 {
        use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
        use crate::vtable::{Shape, VtableTarget};
        struct NoFacts;
        impl LayoutFacts for NoFacts {
            fn adt(&self, _: ItemKey) -> Option<blaze_typeck::Adt<'_>> {
                None
            }
            fn repr(&self, item: ItemKey) -> Result<ReprAttrs, Missing> {
                Err(Missing { item })
            }
            fn discriminant(&self, item: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
                Err(Missing { item })
            }
            fn valid_range(&self, item: ItemKey) -> Result<ValidRange, Missing> {
                Err(Missing { item })
            }
        }
        let alloc = blaze_own::testing::heap().alloc();
        let facts = self.facts;
        let world = NoFacts;
        let mut stack = Vec::new_in(Global);
        stack.push(want);
        while let Some(&t) = stack.last() {
            if self.placed.iter().any(|(r, _)| *r == t) {
                stack.pop();
                continue;
            }
            let shape = Shape::of(t.principal, facts, alloc).expect("the test's trait facts");
            let &(_, size, align, glue) = self.sizes.iter().find(|s| s.0 == t.concrete).expect("the concrete type's size in the test");
            let args = self.ground.view().list(t.args).expect("the table's arguments").to_vec();
            // Word 0: `drop_in_place::<T>`, the glue instance at the concrete type.
            let glue_args = self.ground.intern_list(&[t.concrete]).expect("the glue's arguments");
            let drop = glue.map(|item| Instance { item, args: glue_args });
            let table = crate::vtable::Vtable::build(&shape, t.concrete, &args, size, align, drop, self.ground, &world, alloc, Global).expect("the table");
            let mut pushed_super = false;
            for &(_, to) in table.relocs.iter() {
                if let VtableTarget::Super(sup) = to
                    && !self.placed.iter().any(|(r, _)| *r == sup)
                    && !stack.contains(&sup)
                {
                    stack.push(sup);
                    pushed_super = true;
                }
            }
            if pushed_super {
                continue;
            }
            let words: std::vec::Vec<u64> = table.bytes.chunks(8).map(|c| u64::from_le_bytes(c.try_into().expect("a word"))).collect();
            let words: &'static mut [u64] = std::vec::Vec::leak(words);
            for &(offset, to) in table.relocs.iter() {
                words[offset as usize / 8] = match to {
                    VtableTarget::Fn(i) if Some(i.item) == glue => fn_addr(i.item, Some(i.args)),
                    VtableTarget::Fn(i) => {
                        let &(_, _, f) = self
                            .impls
                            .iter()
                            .find(|&&(m, c, _)| m == i.item && c == t.concrete)
                            .unwrap_or_else(|| panic!("no impl fn of {:?} for the table's type in the test", i.item));
                        fn_addr(f, None)
                    }
                    VtableTarget::Super(sup) => self.placed.iter().find(|(r, _)| *r == sup).expect("placed above").1,
                };
            }
            self.placed.push((t, words.as_ptr() as u64));
            stack.pop();
        }
        self.placed.iter().find(|(r, _)| *r == want).expect("placed").1
    }
}

/// [`run_native_with`], placing the vtables each fn names from `tables`.
#[cfg(target_arch = "aarch64")]
fn run_native_vt(host: &[(ItemKey, u64)], fns: &[(ItemKey, Option<List>, &Built<Global>)], mut tables: Option<&mut Tables<'_>>) -> u64 {
    use blaze_emit::RelocTo;
    use blaze_inproc::exec::Executable;
    let mut placed: Vec<(ItemKey, Option<List>, Executable), Global> = Vec::new_in(Global);
    for &(key, key_args, built) in fns {
        let mut vias = Vec::new_in(Global);
        vias.resize(built.refs.len(), blaze_emit::Via::Address);
        let mut code = blaze_emit::Code::new_in(Global);
        let isa = blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs };
        blaze_emit::emit(built.func.verified(), &vias, isa, &mut code).unwrap_or_else(|e| panic!("{e}"));
        let mut table_at: Vec<(u32, u64), Global> = Vec::new_in(Global);
        for r in code.relocs.iter() {
            if let RelocTo::Vtable(p) = r.to {
                let fn_addr = |item: ItemKey, args: Option<List>| -> u64 {
                    placed
                        .iter()
                        .find(|(k, a, _)| *k == item && (args.is_none() || *a == args))
                        .unwrap_or_else(|| panic!("{item:?} at {args:?} is not placed before the table naming it"))
                        .2
                        .code_address()
                };
                let t = tables.as_deref_mut().expect("a fn names a vtable and the run was given no tables");
                table_at.push((p, t.place(built.vtables[p as usize], &fn_addr)));
            }
        }
        let callee = |place: u32| -> u64 {
            let Instance { item, args } = built.refs[place as usize];
            if let Some(&(_, at)) = host.iter().find(|(k, _)| *k == item) {
                return at;
            }
            let (_, _, exec) = placed
                .iter()
                .find(|(k, a, _)| *k == item && a.is_none_or(|a| a == args))
                .unwrap_or_else(|| panic!("callee {item:?} at {args:?} is not placed before its caller"));
            exec.code_address()
        };
        let exec = Executable::written(code.bytes.len(), 0, |out, _| {
            let base = out.as_ptr() as u64;
            out.copy_from_slice(&code.bytes);
            for f in code.fixups.iter() {
                let at = f.at as usize;
                out[at..at + 8].copy_from_slice(&callee(f.place).to_le_bytes());
            }
            for r in code.relocs.iter() {
                let to = match r.to {
                    RelocTo::Code(off) => base + u64::from(off),
                    RelocTo::Func(p) => callee(p),
                    RelocTo::Vtable(p) => table_at.iter().find(|&&(q, _)| q == p).expect("the table placed above").1,
                    RelocTo::Routine(routine) => wide::routine_at(routine),
                    other => panic!("the native run places no {other:?}"),
                };
                let at = r.at as usize;
                out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
            }
        })
        .expect("the code maps");
        placed.push((key, key_args, exec));
    }
    let (_, _, entry) = placed.last().expect("one fn to run");
    // SAFETY: the entry is an emitted function of no parameters returning one integer in x0;
    // every callee it names is mapped and held in `placed` until it returns.
    let f: extern "C" fn() -> u64 = unsafe { entry.as_fn() };
    f()
}

/// lib.rs, "Adjustments": `&[u8; 3]` to `&[u8]` (s1-layout's `let wide: &[u8] = &bytes;`), and
/// its `&mut` and raw-pointer forms, each the pair of the array's own address (never a copy)
/// and 3:
/// ```text
/// fn main() -> usize {
///     let mut a: [u8; 3] = [7, 2, 3];
///     let s: &mut [u8] = &mut a; let t: (*mut u8, usize) = transmute(s); *t.0 = 9;
///     let r: *const [u8] = &a;   let u: (*const u8, usize) = transmute(r);
///     let w: &[u8] = &a;         let v: (*const u8, usize) = transmute(w);
///     (*v.0 as usize) * 10000 + t.1 * 100 + u.1 * 10 + v.1      // 90333
/// }
/// ```
#[test]
fn an_array_reference_unsizes_to_a_slice_pair() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let arr_ty = b.types.array_n(u8_ty, 3).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let mut_slice = b.types.intern(PNode::Ref(Mutbl::Mut, slice)).expect("a type interns");
    let raw_slice = b.types.intern(PNode::Ptr(Mutbl::Not, slice)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let pair = |b: &mut BodyBuf, m: Mutbl| {
        let p = b.types.intern(PNode::Ptr(m, u8_ty)).expect("a type interns");
        let l = b.types.intern_list(&[p, usize_ty]).expect("a list interns");
        b.types.intern(PNode::Tuple(l)).expect("a type interns")
    };
    let mut_pair = pair(&mut b, Mutbl::Mut);
    let const_pair = pair(&mut b, Mutbl::Not);
    let (a, s, t, r, u, sw, v) = (b.bind("a"), b.bind("s"), b.bind("t"), b.bind("r"), b.bind("u"), b.bind("w"), b.bind("v"));
    let e7 = b.int(7, None);
    let e2 = b.int(2, None);
    let e3 = b.int(3, None);
    let arr = b.array(&[e7, e2, e3]);
    let a_e = b.local(a);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: a_e });
    let s_e = b.local(s);
    let t_init = b.call("transmute", &[s_e]);
    let t_e = b.local(t);
    let t0 = b.tfield(t_e, 0);
    let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: t0 });
    let nine = b.int(9, None);
    let write = b.e(Expr::Assign { lhs: star, rhs: nine });
    let a_e = b.local(a);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let r_e = b.local(r);
    let u_init = b.call("transmute", &[r_e]);
    let a_e = b.local(a);
    let w_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let w_e = b.local(sw);
    let v_init = b.call("transmute", &[w_e]);
    let v_e = b.local(v);
    let v0 = b.tfield(v_e, 0);
    let first = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: v0 });
    let first = b.e(Expr::Cast { operand: first, ty: usize_ty });
    let k = b.int(10000, None);
    let x = b.bin(BinOp::Mul, first, k);
    let t_e = b.local(t);
    let t1 = b.tfield(t_e, 1);
    let k = b.int(100, None);
    let y = b.bin(BinOp::Mul, t1, k);
    let u_e = b.local(u);
    let u1 = b.tfield(u_e, 1);
    let k = b.int(10, None);
    let z = b.bin(BinOp::Mul, u1, k);
    let v_e = b.local(v);
    let v1 = b.tfield(v_e, 1);
    let sum = b.bin(BinOp::Add, x, y);
    let sum = b.bin(BinOp::Add, sum, z);
    let sum = b.bin(BinOp::Add, sum, v1);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: Some(arr_ty), init: Some(arr), else_: None },
            Stmt::Let { pat: s, ty: Some(mut_slice), init: Some(s_init), else_: None },
            Stmt::Let { pat: t, ty: Some(mut_pair), init: Some(t_init), else_: None },
            Stmt::Expr { expr: write, semi: true },
            Stmt::Let { pat: r, ty: Some(raw_slice), init: Some(r_init), else_: None },
            Stmt::Let { pat: u, ty: Some(const_pair), init: Some(u_init), else_: None },
            Stmt::Let { pat: sw, ty: Some(ref_slice), init: Some(w_init), else_: None },
            Stmt::Let { pat: v, ty: Some(const_pair), init: Some(v_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    // The LIR: no call (an unsizing is two stores), three lengths of 3 written beside the
    // address, each pair a 16-byte temporary of its own.
    assert!(!text.contains("call"), "{text}");
    assert_eq!(text.lines().filter(|l| l.contains("iconst") && l.ends_with(" 3")).count(), 4, "{text}");
    // Native only: the evaluator stores no pointer in memory.
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 90333);
}

/// lib.rs, "Adjustments": overloaded dereferences through `impl Deref for Wrap { type Target
/// = Foo; }`, whose `deref` is `fn wrap_deref(w: &Wrap) -> &Foo { &w.inner }`:
/// ```text
/// fn main() -> u32 {
///     let w = Wrap { inner: Foo { n: 40 } };
///     let r: &Foo = &w;          // deref, deref (overloaded), borrow: a reborrow of the call
///     r.n + (*w).n + w.n + 2     // `*w` overloaded as a place; `w.n` an autoderef of the base
/// }
/// ```
/// Each overloaded step calls `deref` with `&w` borrowed in place: three calls, 122.
#[test]
fn an_overloaded_deref_calls_deref_with_the_place() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.strukt(10, "Foo", &[("n", U32)]);
    let foo = PNode::Adt(ItemKey(10), List::EMPTY);
    w.strukt(11, "Wrap", &[("inner", foo)]);
    let wrap = PNode::Adt(ItemKey(11), List::EMPTY);
    let r_wrap = w.ref_(Mutbl::Not, wrap);
    let r_foo = w.ref_(Mutbl::Not, foo);
    w.func(12, "wrap_deref", &[r_wrap], r_foo);
    w.deref_impl(wrap, foo, ItemKey(12));

    let mut callee = BodyBuf::new();
    let p = callee.bind("w");
    callee.params.push(p);
    let p_e = callee.local(p);
    let inner = callee.field(p_e, "inner");
    let r = callee.e(Expr::Ref { mutbl: Mutbl::Not, operand: inner });
    let root = callee.block(&[], Some(r));
    let mut g = PortableTys::new_in(Alloc::global());
    let deref = build_instance(&w, &callee, ItemKey(12), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));

    let mut b = BodyBuf::new();
    let foo_ty = b.types.intern(foo).expect("a type interns");
    let ref_foo = b.types.intern(PNode::Ref(Mutbl::Not, foo_ty)).expect("a type interns");
    let wv = b.bind("w");
    let rv = b.bind("r");
    let forty = b.int(40, None);
    let foo_path = b.path("Foo");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("n"), expr: forty });
    let foo_lit = b.e(Expr::Struct { path: foo_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let wrap_path = b.path("Wrap");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("inner"), expr: foo_lit });
    let wrap_lit = b.e(Expr::Struct { path: wrap_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let w_e = b.local(wv);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: w_e });
    let r_e = b.local(rv);
    let rn = b.field(r_e, "n");
    let w_e = b.local(wv);
    let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: w_e });
    let paren = b.e(Expr::Paren(star));
    let sn = b.field(paren, "n");
    let w_e = b.local(wv);
    let wn = b.field(w_e, "n");
    let two = b.int(2, None);
    let sum = b.bin(BinOp::Add, rn, sn);
    let sum = b.bin(BinOp::Add, sum, wn);
    let sum = b.bin(BinOp::Add, sum, two);
    let root = b.block(
        &[
            Stmt::Let { pat: wv, ty: None, init: Some(wrap_lit), else_: None },
            Stmt::Let { pat: rv, ty: Some(ref_foo), init: Some(r_init), else_: None },
        ],
        Some(sum),
    );
    let mut g = PortableTys::new_in(Alloc::global());
    let built = build_instance(&w, &b, MAIN, root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    // The LIR: one reference, to `deref` at `[Wrap]`, called three times with a pointer.
    assert_eq!(built.refs.len(), 1, "{text}");
    assert_eq!(built.refs[0].item, ItemKey(12));
    assert_eq!(text.lines().filter(|l| l.contains(" call ")).count(), 3, "{text}");
    assert!(has_call_sig(&built, &[Ty::PTR], &[Ty::PTR]), "{text}");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(ItemKey(12), &deref), (MAIN, &built)]) as u32, 122);
}

/// lib.rs, "Adjustments": a `&mut` borrow through a dereference the check recorded as
/// `Deref::deref` is refused by name, never built through a shared reference.
#[test]
fn a_mut_borrow_through_deref_not_deref_mut_is_refused() {
    let mut w = TestWorld::new();
    w.strukt(10, "Foo", &[("n", U32)]);
    let foo = PNode::Adt(ItemKey(10), List::EMPTY);
    w.strukt(11, "Wrap", &[("inner", foo)]);
    let wrap = PNode::Adt(ItemKey(11), List::EMPTY);
    let r_wrap = w.ref_(Mutbl::Not, wrap);
    let r_foo = w.ref_(Mutbl::Not, foo);
    w.func(12, "wrap_deref", &[r_wrap], r_foo);
    w.deref_impl(wrap, foo, ItemKey(12));
    // `fn main(m: &mut Wrap) { let f: &mut Foo = m; }`
    let m_wrap = w.ref_(Mutbl::Mut, wrap);
    w.func(1, "main", &[m_wrap], UNIT);
    let mut b = BodyBuf::new();
    let foo_ty = b.types.intern(foo).expect("a type interns");
    let mut_foo = b.types.intern(PNode::Ref(Mutbl::Mut, foo_ty)).expect("a type interns");
    let m = b.bind("m");
    let f = b.bind("f");
    b.params.push(m);
    let m_e = b.local(m);
    let root = b.block(&[Stmt::Let { pat: f, ty: Some(mut_foo), init: Some(m_e), else_: None }], None);
    let error = build(&w, &b, root).err().expect("the build refuses");
    assert!(error.contains("`DerefMut::deref_mut`"), "{error}");
}

/// Rung 7a's shape, run: four locals whose types need drop, one of them a struct holding a `G`,
/// one moved into another local. `let a = G{1}; let b = B { n: 7, g: G{2} }; let c = G{3};
/// let d = G{4}; let e = d;` drops e (4), c (3), b's g (2), a (1), each once, in that order.
#[cfg(target_arch = "aarch64")]
#[test]
fn four_locals_drop_in_reverse_order_when_run() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.strukt(80, "B", &[("n", I32), ("g", g)]);
    let bt = PNode::Adt(ItemKey(80), List::EMPTY);
    let mut b = BodyBuf::new();
    let (a, bb, c, d, e) = (b.bind("a"), b.bind("b"), b.bind("c"), b.bind("d"), b.bind("e"));
    let a_init = g_lit(&mut b, 1);
    let seven = b.int(7, None);
    let g2 = g_lit(&mut b, 2);
    let b_path = b.path("B");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("n"), expr: seven });
    b.field_inits.push(FieldInit { name: Name::of("g"), expr: g2 });
    let b_init = b.e(Expr::Struct { path: b_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let c_init = g_lit(&mut b, 3);
    let d_init = g_lit(&mut b, 4);
    let e_init = b.local(d);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: bb, ty: None, init: Some(b_init), else_: None },
            Stmt::Let { pat: c, ty: None, init: Some(c_init), else_: None },
            Stmt::Let { pat: d, ty: None, init: Some(d_init), else_: None },
            Stmt::Let { pat: e, ty: None, init: Some(e_init), else_: None },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));

    let mut gb = BodyBuf::new();
    let p = gb.bind("to_drop");
    gb.params.push(p);
    let groot = gb.block(&[], None);
    let view = gb.view(DROP_IN_PLACE, groot);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let mut glue = |t: PNode| -> (List, Built<Global>) {
        let ty = ground.intern(t).expect("intern");
        let args = ground.intern_list(&[ty]).expect("intern");
        let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        (args, built)
    };
    let (g_args, g_glue) = glue(g);
    let (b_args, b_glue) = glue(bt);
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(
        &[(G_DROP, host_g_drop as usize as u64)],
        &[(DROP_IN_PLACE, Some(g_args), &g_glue), (DROP_IN_PLACE, Some(b_args), &b_glue), (MAIN, None, &main)],
    );
    let seen = DROPPED.with(|d| d.borrow().clone());
    assert_eq!(seen.as_slice(), &[4, 3, 2, 1], "{}", blaze_lir::print(main.func.view()));
}

/// bstd's `match &self.cancel { Some(c) => .., None => .. }` on an enum that needs drop: the
/// match goes through the reference (the default binding mode is `ref`), `c` is the field's
/// address, nothing is moved or dropped by the match. With `enum O3 { N, S(G) }`:
/// `let a = S(G{5}); let b = N; let ra = &a;
///  (match ra { S(c) => c.x, N => 0 }) + match &b { S(c) => c.x + 100, N => 1000 }` is 1005,
/// and `a` alone is dropped, once, at the end (its glue sees `x == 5`).
#[test]
fn a_match_through_a_reference_binds_by_reference() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let g = g_world(&mut w);
    w.enum_(85, &[("N", CtorKind::Unit, &[]), ("S", CtorKind::Tuple, &[("0", g)])]);
    let o3 = PNode::Adt(ItemKey(85), List::EMPTY);
    let mut b = BodyBuf::new();
    let (a, bl, ra) = (b.bind("a"), b.bind("b"), b.bind("ra"));
    let g5 = g_lit(&mut b, 5);
    let a_init = b.call("S", &[g5]);
    let n_path = b.path("N");
    let b_init = b.e(Expr::Path(n_path));
    let a_e = b.local(a);
    let ra_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    // Each match: `S(c) => c.x + plus, N => none`.
    let arms = |b: &mut BodyBuf, plus: u128, none: u128| -> List {
        // A plain `c`: edition 2024 refuses `mut` where the default binding mode is `ref`.
        let c = b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("c"), sub: None, ty: None });
        let s_path = b.path("S");
        let elems = b.pats_list(&[c]);
        let ps = b.pat(Pat::TupleStruct { path: s_path, elems, rest: None });
        let pn_path = b.path("N");
        let pn = b.pat(Pat::Path(pn_path));
        let c_e = b.local(c);
        let cx = b.field(c_e, "x");
        let k = b.int(plus, None);
        let some = b.bin(BinOp::Add, cx, k);
        let none = b.int(none, None);
        let start = b.arms.len() as u32;
        b.arms.push(Arm { pat: ps, guard: None, body: some });
        b.arms.push(Arm { pat: pn, guard: None, body: none });
        List { start, len: 2 }
    };
    let first = arms(&mut b, 0, 0);
    let ra_e = b.local(ra);
    let m1 = b.e(Expr::Match { scrutinee: ra_e, arms: first });
    let second = arms(&mut b, 100, 1000);
    let b_e = b.local(bl);
    let rb = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: b_e });
    let m2 = b.e(Expr::Match { scrutinee: rb, arms: second });
    let sum = b.bin(BinOp::Add, m1, m2);
    let o3_ty = b.types.intern(o3).expect("a type interns");
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: bl, ty: Some(o3_ty), init: Some(b_init), else_: None },
            Stmt::Let { pat: ra, ty: None, init: Some(ra_init), else_: None },
        ],
        Some(sum),
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    // Two drops, a's and b's (O3 needs drop as a type), and no others.
    let glue_calls: usize =
        main.refs.iter().enumerate().filter(|(_, r)| r.item == DROP_IN_PLACE).map(|(i, _)| calls_of(&main, i)).sum();
    assert_eq!(glue_calls, 2, "{text}");

    #[cfg(target_arch = "aarch64")]
    {
        let mut gb = BodyBuf::new();
        let p = gb.bind("to_drop");
        gb.params.push(p);
        let groot = gb.block(&[], None);
        let view = gb.view(DROP_IN_PLACE, groot);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        let sig = w.sig(DROP_IN_PLACE).expect("a signature");
        let mut glue = |t: PNode| -> (List, Built<Global>) {
            let ty = ground.intern(t).expect("intern");
            let args = ground.intern_list(&[ty]).expect("intern");
            let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
            let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
                .unwrap_or_else(|e| panic!("{e}"));
            (args, built)
        };
        let (g_args, g_glue) = glue(g);
        let (o_args, o_glue) = glue(o3);
        DROPPED.with(|d| d.borrow_mut().clear());
        let got = run_native_with(
            &[(G_DROP, host_g_drop as usize as u64)],
            &[(DROP_IN_PLACE, Some(g_args), &g_glue), (DROP_IN_PLACE, Some(o_args), &o_glue), (MAIN, None, &main)],
        );
        assert_eq!(got as u32, 1005, "{text}");
        let seen = DROPPED.with(|d| d.borrow().clone());
        assert_eq!(seen.as_slice(), &[5]);
    }
}

/// `struct M { g: G }` answered as `ManuallyDrop`: `let m = M { g: G { x: 1 } }; let k = G { x: 2 };`
/// drops `k` only. Without the answer, `m`'s glue is called too (the control).
#[test]
fn a_manually_drop_field_is_never_dropped() {
    let glue_calls = |lang: bool| {
        let mut w = TestWorld::new();
        w.func(1, "main", &[], UNIT);
        let g = g_world(&mut w);
        w.strukt(70, "M", &[("g", g)]);
        if lang {
            w.manually_drop = Some(ItemKey(70));
        }
        let mut b = BodyBuf::new();
        let (m, k) = (b.bind("m"), b.bind("k"));
        let g1 = g_lit(&mut b, 1);
        let m_path = b.path("M");
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("g"), expr: g1 });
        let m_init = b.e(Expr::Struct { path: m_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
        let k_init = g_lit(&mut b, 2);
        let root = b.block(
            &[
                Stmt::Let { pat: m, ty: None, init: Some(m_init), else_: None },
                Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None },
            ],
            None,
        );
        let mut ground = PortableTys::new_in(Alloc::global());
        let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
        main.refs.iter().enumerate().filter(|(_, r)| r.item == DROP_IN_PLACE).map(|(i, _)| calls_of(&main, i)).sum::<usize>()
    };
    assert_eq!(glue_calls(true), 1, "k only");
    assert_eq!(glue_calls(false), 2, "the control: m and k");
}

#[cfg(target_arch = "aarch64")]
std::thread_local! {
    /// What the host `Drop` fns of `enum_glue_drops_the_live_variant` saw, in order.
    static DROPPED: core::cell::RefCell<Vec<i32, Global>> = core::cell::RefCell::new(Vec::new_in(Global));
}

/// `<G as Drop>::drop(&mut self)` on the host: logs `self.x` (G's only field, at offset 0).
#[cfg(target_arch = "aarch64")]
extern "C" fn host_g_drop(g: *mut i32) {
    // SAFETY: the emitted glue passes the address of a live `G`, whose `x: i32` is at 0.
    let x = unsafe { *g };
    DROPPED.with(|d| d.borrow_mut().push(x));
}

/// `<R as Drop>::drop(&mut self)` on the host: logs `*self.r` (R's only field, `&i32`).
#[cfg(target_arch = "aarch64")]
extern "C" fn host_r_drop(r: *mut *const i32) {
    // SAFETY: the emitted glue passes the address of a live `R`, whose `r: &i32` is at 0 and
    // points to a local of `main`, alive until `main` returns.
    let x = unsafe { **r };
    DROPPED.with(|d| d.borrow_mut().push(x + 1000));
}

/// Class M (hello-1116 stops-3-briefs.md): a union's bytes are copied whole, never field by
/// field. The Reference, "Unions": "Unions have no notion of an 'active field'. Instead, every
/// union access just interprets the storage as the type of the field used for the access", so a
/// copy may not read any field's tag: the bytes past the written field are uninitialised.
/// ```text
/// enum E { A(u64), B(u32) }   union U { a: u8, b: E }
/// fn main() -> u8 { let u = U { a: 7 }; let v = u; v.a }   // 7
/// ```
/// A copy that walked `b` switches on E's tag, read from bytes `U { a: 7 }` never wrote (the
/// run's trap in a built `MaybeUninit::<Later>::uninit`): it fails here with the evaluator's
/// refusal or the `unreachable` default. Prints "union copy: 7, no switch".
#[test]
fn a_union_copies_its_bytes_not_a_field() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U8);
    w.enum_(40, &[("A", CtorKind::Tuple, &[("0", PNode::Uint(UintTy::U64))]), ("B", CtorKind::Tuple, &[("0", PNode::Uint(UintTy::U32))])]);
    let e = PNode::Adt(ItemKey(40), List::EMPTY);
    w.strukt(60, "U", &[("a", U8), ("b", e)]);
    w.structs.last_mut().expect("U").kind = AdtKind::Union;
    let mut b = BodyBuf::new();
    let (u, v) = (b.bind("u"), b.bind("v"));
    let u_path = b.path("U");
    let seven = b.int(7, None);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: seven });
    let u_init = b.e(Expr::Struct { path: u_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let u_e = b.local(u);
    let v_e = b.local(v);
    let va = b.field(v_e, "a");
    let root = b.block(
        &[
            Stmt::Let { pat: u, ty: None, init: Some(u_init), else_: None },
            Stmt::Let { pat: v, ty: None, init: Some(u_e), else_: None },
        ],
        Some(va),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(!text.contains("switch") && !text.contains("unreachable"), "the copy switched on a tag:\n{text}");
    assert_eq!(one(&built, &[]), 7, "{text}");
    println!("union copy: 7, no switch");
}

/// lib.rs, "Drops": the glue of an enum drops its live variant's fields, in declaration order;
/// a variant with nothing to drop (and a niche's empty variant) drops nothing; a union drops no
/// field. With `G { x: i32 }` and `R { r: &i32 }` each with a host `Drop`:
/// ```text
/// enum E2 { A, B(G, i32, G), C(G) }   enum O { N, S(R) }   // O's `N` is R's null niche
/// union U { g: G, n: i32 }
/// fn main() {
///     let seven = 7;
///     let a = B(G { x: 1 }, 5, G { x: 2 }); let c = C(G { x: 3 }); let n = A;
///     let s = S(R { r: &seven }); let o = N;
///     let u = U { n: 4 };
/// }   // dropped in reverse: u (nothing), o (nothing), s (1007), n (nothing), c (3), a (1, 2)
/// ```
#[test]
fn enum_glue_drops_the_live_variant() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let r_i32 = w.ref_(Mutbl::Not, I32);
    w.strukt(55, "R", &[("r", r_i32)]);
    let r = PNode::Adt(ItemKey(55), List::EMPTY);
    w.drops.push((ItemKey(55), ItemKey(56)));
    w.enum_(40, &[("A", CtorKind::Unit, &[]), ("B", CtorKind::Tuple, &[("0", g), ("1", I32), ("2", g)]), ("C", CtorKind::Tuple, &[("0", g)])]);
    w.enum_(50, &[("N", CtorKind::Unit, &[]), ("S", CtorKind::Tuple, &[("0", r)])]);
    w.strukt(60, "U", &[("g", g), ("n", I32)]);
    w.structs.last_mut().expect("U").kind = AdtKind::Union;
    let e2 = PNode::Adt(ItemKey(40), List::EMPTY);
    let o = PNode::Adt(ItemKey(50), List::EMPTY);
    let u = PNode::Adt(ItemKey(60), List::EMPTY);

    let mut b = BodyBuf::new();
    let o_ty = b.types.intern(o).expect("a type interns");
    let (seven, a, c, n, s, on, un) = (b.bind("seven"), b.bind("a"), b.bind("c"), b.bind("n"), b.bind("s"), b.bind("o"), b.bind("u"));
    let seven_lit = b.int(7, None);
    let g1 = g_lit(&mut b, 1);
    let five = b.int(5, None);
    let g2 = g_lit(&mut b, 2);
    let a_init = b.call("B", &[g1, five, g2]);
    let g3 = g_lit(&mut b, 3);
    let c_init = b.call("C", &[g3]);
    let a_path = b.path("A");
    let n_init = b.e(Expr::Path(a_path));
    let seven_e = b.local(seven);
    let r_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: seven_e });
    let r_path = b.path("R");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("r"), expr: r_ref });
    let r_lit = b.e(Expr::Struct { path: r_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let s_init = b.call("S", &[r_lit]);
    let n_path = b.path("N");
    let o_init = b.e(Expr::Path(n_path));
    let u_path = b.path("U");
    let four = b.int(4, None);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("n"), expr: four });
    let u_init = b.e(Expr::Struct { path: u_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let root = b.block(
        &[
            Stmt::Let { pat: seven, ty: None, init: Some(seven_lit), else_: None },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: c, ty: None, init: Some(c_init), else_: None },
            Stmt::Let { pat: n, ty: None, init: Some(n_init), else_: None },
            Stmt::Let { pat: s, ty: None, init: Some(s_init), else_: None },
            Stmt::Let { pat: on, ty: Some(o_ty), init: Some(o_init), else_: None },
            Stmt::Let { pat: un, ty: None, init: Some(u_init), else_: None },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    // a, c and s are dropped (their glue called); n, o and u are not: E2 and O need drop as
    // types, so n's and o's glue is called too, and drops nothing at run time.
    let glue_calls: usize =
        main.refs.iter().enumerate().filter(|(_, r)| r.item == DROP_IN_PLACE).map(|(i, _)| calls_of(&main, i)).sum();
    assert_eq!(glue_calls, 5, "{text}");

    // The glue of each type, its instance at that type, in the one ground table.
    let mut gb = BodyBuf::new();
    let p = gb.bind("to_drop");
    gb.params.push(p);
    let groot = gb.block(&[], None);
    let view = gb.view(DROP_IN_PLACE, groot);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let mut glue = |t: PNode| -> (List, Built<Global>) {
        let ty = ground.intern(t).expect("intern");
        let args = ground.intern_list(&[ty]).expect("intern");
        let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        emits(&built, &[]);
        (args, built)
    };
    let (g_args, g_glue) = glue(g);
    let (r_args, r_glue) = glue(r);
    let (e2_args, e2_glue) = glue(e2);
    let (o_args, o_glue) = glue(o);
    let (u_args, u_glue) = glue(u);
    // The LIR: E2's glue switches on the discriminant to B's two drops and C's one; U's glue
    // is empty.
    let e2_text = blaze_lir::print(e2_glue.func.view());
    assert!(e2_text.contains("switch"), "{e2_text}");
    assert_eq!(calls_of(&e2_glue, 0), 3, "{e2_text}");
    assert!(!blaze_lir::print(u_glue.func.view()).contains("call"));

    #[cfg(target_arch = "aarch64")]
    {
        let host = [(G_DROP, host_g_drop as usize as u64), (ItemKey(56), host_r_drop as usize as u64)];
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(
            &host,
            &[
                (DROP_IN_PLACE, Some(g_args), &g_glue),
                (DROP_IN_PLACE, Some(r_args), &r_glue),
                (DROP_IN_PLACE, Some(e2_args), &e2_glue),
                (DROP_IN_PLACE, Some(o_args), &o_glue),
                (DROP_IN_PLACE, Some(u_args), &u_glue),
                (MAIN, None, &main),
            ],
        );
        let seen = DROPPED.with(|d| d.borrow().clone());
        assert_eq!(seen.as_slice(), &[1007, 3, 1, 2]);
    }
}

/// The world of the by-value match tests (codegen.md, "A `match` by value on an enum that
/// needs drop"): `G` (its host `Drop` logs `x`), `enum N { P(G), Q }` and
/// `enum E { A(G, G), B(G), C, W(N, G) }`; returns `G`, `N` and `E`.
fn match_world(w: &mut TestWorld) -> (PNode, PNode, PNode) {
    w.func(1, "main", &[], UNIT);
    let g = g_world(w);
    // (`enum_` keys variant `i` as the enum's key + 1 + i.)
    w.enum_(110, &[("P", CtorKind::Tuple, &[("0", g)]), ("Q", CtorKind::Unit, &[])]);
    let n = PNode::Adt(ItemKey(110), List::EMPTY);
    w.enum_(
        100,
        &[
            ("A", CtorKind::Tuple, &[("0", g), ("1", g)]),
            ("B", CtorKind::Tuple, &[("0", g)]),
            ("C", CtorKind::Unit, &[]),
            ("W", CtorKind::Tuple, &[("0", n), ("1", g)]),
        ],
    );
    (g, n, PNode::Adt(ItemKey(100), List::EMPTY))
}

/// The scrutinee's value: `A(G{1}, G{2})`, `B(G{3})`, `C` or `W(P(G{5}), G{6})`.
#[derive(Clone, Copy, Debug)]
enum Scrut {
    A,
    B,
    C,
    W,
}

/// The arms: `Each` is `A(a, _) | B(b) | C | W(_, g)`, each `=> {}`; `Whole` is
/// `A(_, b) | C | y`; `Wild` is `W(n, _) | _`; `Return` is `A(a, _) => return, _ => {}`;
/// `Deep` is `W(P(s), _) | _`; `Guard(t)` is `A(a, _) if 1 == 1 | _` (`t`), else with
/// `1 == 2`; `DeepGuard(t)` is `W(P(s), _)` guarded alike `| _`; `Or` is
/// `A(_, s) | B(s) => {}, _ => {}`, `OrGuard` the same with `if 1 == 1` on the first arm.
#[derive(Clone, Copy, Debug)]
enum Arms {
    Or,
    OrGuard,
    Each,
    Whole,
    Wild,
    Return,
    Deep,
    Guard(bool),
    DeepGuard(bool),
}

/// `name(subs..)` as a pattern.
fn variant_pat(b: &mut BodyBuf, name: &str, subs: &[PatId]) -> PatId {
    let path = b.path(name);
    let elems = b.pats_list(subs);
    b.pat(Pat::TupleStruct { path, elems, rest: None })
}

fn scrut_value(b: &mut BodyBuf, s: Scrut) -> ExprId {
    match s {
        Scrut::A => {
            let (g1, g2) = (g_lit(b, 1), g_lit(b, 2));
            b.call("A", &[g1, g2])
        }
        Scrut::B => {
            let g3 = g_lit(b, 3);
            b.call("B", &[g3])
        }
        Scrut::C => {
            let path = b.path("C");
            b.e(Expr::Path(path))
        }
        Scrut::W => {
            let g5 = g_lit(b, 5);
            let p = b.call("P", &[g5]);
            let g6 = g_lit(b, 6);
            b.call("W", &[p, g6])
        }
    }
}

fn arm_list(b: &mut BodyBuf, arms: Arms) -> List {
    let mut list: Vec<Arm, Global> = Vec::new_in(Global);
    let mut push = |b: &mut BodyBuf, pat: PatId, ret: bool| {
        let body = if ret { b.e(Expr::Return { value: None }) } else { b.block(&[], None) };
        list.push(Arm { pat, guard: None, body });
    };
    match arms {
        Arms::Or | Arms::OrGuard => {
            let (w0, s) = (b.pat(Pat::Wild), b.bind("s"));
            let pa = variant_pat(b, "A", &[w0, s]);
            let s2 = b.bind("s");
            let pb = variant_pat(b, "B", &[s2]);
            let alts = b.pats_list(&[pa, pb]);
            let or = b.pat(Pat::Or { alts });
            push(b, or, false);
            let w1 = b.pat(Pat::Wild);
            push(b, w1, false);
            if let Arms::OrGuard = arms {
                let (one, other) = (b.int(1, None), b.int(1, None));
                list[0].guard = Some(b.bin(BinOp::Eq, one, other));
            }
        }
        Arms::Each => {
            let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
            let pa = variant_pat(b, "A", &[a, w0]);
            push(b, pa, false);
            let bb = b.bind("b");
            let pb = variant_pat(b, "B", &[bb]);
            push(b, pb, false);
            let c = b.path("C");
            let pc = b.pat(Pat::Path(c));
            push(b, pc, false);
            let (w1, g) = (b.pat(Pat::Wild), b.bind("g"));
            let pw = variant_pat(b, "W", &[w1, g]);
            push(b, pw, false);
        }
        Arms::Whole => {
            let (w0, bb) = (b.pat(Pat::Wild), b.bind("b"));
            let pa = variant_pat(b, "A", &[w0, bb]);
            push(b, pa, false);
            let c = b.path("C");
            let pc = b.pat(Pat::Path(c));
            push(b, pc, false);
            let y = b.bind("y");
            push(b, y, false);
        }
        Arms::Wild | Arms::Deep | Arms::DeepGuard(_) => {
            let (first, w0) = match arms {
                Arms::Wild => (b.bind("n"), b.pat(Pat::Wild)),
                _ => {
                    let s = b.bind("s");
                    (variant_pat(b, "P", &[s]), b.pat(Pat::Wild))
                }
            };
            let pw = variant_pat(b, "W", &[first, w0]);
            push(b, pw, false);
            let w1 = b.pat(Pat::Wild);
            push(b, w1, false);
        }
        Arms::Return | Arms::Guard(_) => {
            let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
            let pa = variant_pat(b, "A", &[a, w0]);
            push(b, pa, matches!(arms, Arms::Return));
            let w1 = b.pat(Pat::Wild);
            push(b, w1, false);
        }
    }
    if let Arms::Guard(passes) | Arms::DeepGuard(passes) = arms {
        let (one, other) = (b.int(1, None), b.int(if passes { 1 } else { 2 }, None));
        list[0].guard = Some(b.bin(BinOp::Eq, one, other));
    }
    let start = b.arms.len() as u32;
    b.arms.extend(list.iter().copied());
    List { start, len: list.len() as u32 }
}

/// `fn main() { let x = s; match x { arms }; let k = G { x: 9 }; }`, or with `temp`
/// `match s { arms }; ..`: built, and with its glue run natively, the order of the drops.
fn match_drops(s: Scrut, arms: Arms, temp: bool) -> Result<Vec<i32, Global>, String> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let e_ty = b.types.intern(e).expect("a type interns");
    let value = scrut_value(&mut b, s);
    let list = arm_list(&mut b, arms);
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let scrutinee = if temp {
        value
    } else {
        let x = b.bind("x");
        stmts.push(Stmt::Let { pat: x, ty: Some(e_ty), init: Some(value), else_: None });
        b.local(x)
    };
    let m = b.e(Expr::Match { scrutinee, arms: list });
    stmts.push(Stmt::Expr { expr: m, semi: true });
    let k = b.bind("k");
    let k_init = g_lit(&mut b, 9);
    stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
    let root = b.block(&stmts, None);
    let seen = g_drops(&w, &b, root, [g, n, e])?;
    if std::env::var_os("BLAZE_TEST_PRINT").is_some() {
        println!("{s:?} {arms:?} temp={temp}: {seen:?}");
    }
    Ok(seen)
}

/// `main` (body `b` from `root`, in `match_world`'s world `w`) built, and run natively with
/// the drop glue of `types` (`G`, `N`, `E`): the order the `G`s were dropped in (each `G`'s
/// `x`). Off aarch64: built only, and nothing seen.
fn g_drops(w: &TestWorld, b: &BodyBuf, root: ExprId, types: [PNode; 3]) -> Result<Vec<i32, Global>, String> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, b, MAIN, root, &[], &mut ground)?;
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (main, types);
        Ok(Vec::new_in(Global))
    }
    #[cfg(target_arch = "aarch64")]
    {
        let [g, n, e] = types;
        let text = blaze_lir::print(main.func.view());
        let mut gb = BodyBuf::new();
        let p = gb.bind("to_drop");
        gb.params.push(p);
        let groot = gb.block(&[], None);
        let view = gb.view(DROP_IN_PLACE, groot);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        let sig = w.sig(DROP_IN_PLACE).expect("a signature");
        // Every other type whose glue `main` names (a tuple of `G`s an `if let`'s failure edge
        // drops whole, lets.rs `link_fail`, though an irrefutable pattern never takes it): its
        // glue is built and placed too, so the run places every callee the code names.
        let mut more: Vec<PNode, Global> = Vec::new_in(Global);
        {
            let v = ground.view();
            for r in main.refs.iter().filter(|r| r.item == DROP_IN_PLACE) {
                let node = v.list(r.args).ok().and_then(|l| l.first().copied()).and_then(|t| v.get(t).copied());
                let Some(node) = node else { return Err(format!("`main` drops at arguments {:?} the table does not hold:\n{text}", r.args)) };
                if ![g, n, e].contains(&node) && !more.contains(&node) {
                    more.push(node);
                }
            }
        }
        let mut glue = |t: PNode| -> (List, Built<Global>) {
            let ty = ground.intern(t).expect("intern");
            let args = ground.intern_list(&[ty]).expect("intern");
            let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
            let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
                .unwrap_or_else(|e| panic!("{e}"));
            (args, built)
        };
        let (g_args, g_glue) = glue(g);
        let (n_args, n_glue) = glue(n);
        let (e_args, e_glue) = glue(e);
        let more: Vec<(List, Built<Global>), Global> = {
            let mut out = Vec::new_in(Global);
            for &t in more.iter() {
                out.push(glue(t));
            }
            out
        };
        // The three first: a glue of another type calls theirs, and is placed before `main`.
        let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
        fns.push((DROP_IN_PLACE, Some(g_args), &g_glue));
        fns.push((DROP_IN_PLACE, Some(n_args), &n_glue));
        fns.push((DROP_IN_PLACE, Some(e_args), &e_glue));
        for (args, built) in more.iter() {
            fns.push((DROP_IN_PLACE, Some(*args), built));
        }
        fns.push((MAIN, None, &main));
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(&[(G_DROP, host_g_drop as usize as u64)], &fns);
        let seen = DROPPED.with(|d| d.borrow().clone());
        if std::env::var_os("BLAZE_TEST_PRINT").is_some() {
            println!("{seen:?}\n{text}");
        }
        Ok(seen)
    }
}

/// codegen.md, "A `match` by value on an enum that needs drop": every field is dropped exactly
/// once, in the Reference's order. A local's arm bindings drop at the arm's end and its rest at
/// its scope's end (after `k`, declared later), by its field flags; a temporary's rest drops
/// at the arm's end, after the arm's bindings (before `k`). Each case: the scrutinee, the arms,
/// local or temporary, and the drops seen (`G`'s `x`; 9 is `k`).
#[test]
fn a_match_by_value_drops_every_field_once() {
    use Arms::*;
    use Scrut::*;
    let cases: &[(Scrut, Arms, bool, &[i32])] = &[
        // Each arm, a binding moved out, the rest at x's end.
        (A, Each, false, &[1, 9, 2]),
        (B, Each, false, &[3, 9]),
        (C, Each, false, &[9]),
        // A nested enum: `W(_, g)` leaves `N`, whose glue drops `P`'s G at x's end.
        (W, Each, false, &[6, 9, 5]),
        // `_` leaves a field; `y` moves the whole value (x's drop flag), dropped at the arm's end.
        (A, Whole, false, &[2, 9, 1]),
        (B, Whole, false, &[3, 9]),
        (W, Whole, false, &[5, 6, 9]),
        // The nested enum moved out whole; the `_` arm moves nothing, x drops all after k.
        (W, Wild, false, &[5, 9, 6]),
        (A, Wild, false, &[9, 1, 2]),
        // An early `return` from an arm drops its binding, then x's rest; k is never made.
        (A, Return, false, &[1, 2]),
        (B, Return, false, &[9, 3]),
        // A temporary: its rest drops at the arm's end, after the arm's bindings.
        (A, Each, true, &[1, 2, 9]),
        (W, Each, true, &[6, 5, 9]),
        (C, Each, true, &[9]),
        (B, Wild, true, &[3, 9]),
        (W, Whole, true, &[5, 6, 9]),
        (A, Return, true, &[1, 2]),
    ];
    let mut failed = String::new();
    for &(s, arms, temp, want) in cases {
        match match_drops(s, arms, temp) {
            Ok(seen) if cfg!(not(target_arch = "aarch64")) || seen.as_slice() == want => {}
            Ok(seen) => failed.push_str(&format!("{s:?} {arms:?} temp={temp}: dropped {seen:?}, want {want:?}\n")),
            Err(e) => failed.push_str(&format!("{s:?} {arms:?} temp={temp}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
}

/// The by-value match's two refusals that need their own world. A move out of an enum that
/// implements `Drop` is E0509, a borrowck error, but no checker on the rebuilt path reports it
/// (only the old layer's hir-ty-borrowck has it), so the builder's refusal is the stop. A move
/// of a field past an enum's 64th needs a second field-flag word.
#[test]
fn a_match_by_value_refuses_drop_impl_enums_and_wide_enums() {
    // `enum E { A(G, G), .. }` with its own `Drop`: `match x { A(a, _) => {}, _ => {} }`.
    let refusal = |w: &mut TestWorld, b: &mut BodyBuf, value: ExprId, pat: PatId, e: PNode| -> String {
        let e_ty = b.types.intern(e).expect("a type interns");
        let x = b.bind("x");
        let w1 = b.pat(Pat::Wild);
        let (u0, u1) = (b.block(&[], None), b.block(&[], None));
        let start = b.arms.len() as u32;
        b.arms.push(Arm { pat, guard: None, body: u0 });
        b.arms.push(Arm { pat: w1, guard: None, body: u1 });
        let xe = b.local(x);
        let m = b.e(Expr::Match { scrutinee: xe, arms: List { start, len: 2 } });
        let root = b.block(&[Stmt::Let { pat: x, ty: Some(e_ty), init: Some(value), else_: None }, Stmt::Expr { expr: m, semi: true }], None);
        let mut ground = PortableTys::new_in(Alloc::global());
        match build_instance(w, b, MAIN, root, &[], &mut ground) {
            Err(e) => e,
            Ok(_) => panic!("the move is refused"),
        }
    };
    let mut w = TestWorld::new();
    let (_, _, e) = match_world(&mut w);
    w.drops.push((ItemKey(100), ItemKey(120)));
    let mut b = BodyBuf::new();
    let value = scrut_value(&mut b, Scrut::A);
    let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
    let pa = variant_pat(&mut b, "A", &[a, w0]);
    let e0509 = refusal(&mut w, &mut b, value, pa, e);
    assert!(e0509.contains("a move out of an enum that implements `Drop` (E0509)"), "{e0509}");

    // `enum V { M(G, G, .. 65 of them) }`: `M(_, .., _, g)` moves the 65th.
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let names: Vec<String, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend((0..65).map(|i| i.to_string()));
        v
    };
    let mut fields: Vec<(&str, PNode), Global> = Vec::new_in(Global);
    fields.extend(names.iter().map(|n| (n.as_str(), g)));
    w.enum_(130, &[("M", CtorKind::Tuple, &fields)]);
    let v = PNode::Adt(ItemKey(130), List::EMPTY);
    let mut b = BodyBuf::new();
    let mut args: Vec<ExprId, Global> = Vec::new_in(Global);
    for i in 0..65 {
        args.push(g_lit(&mut b, i));
    }
    let value = b.call("M", &args);
    let mut subs: Vec<PatId, Global> = Vec::new_in(Global);
    for _ in 0..64 {
        subs.push(b.pat(Pat::Wild));
    }
    subs.push(b.bind("g"));
    let pm = variant_pat(&mut b, "M", &subs);
    let wide = refusal(&mut w, &mut b, value, pm, v);
    assert!(wide.contains("a move out of a field past an enum's 64th"), "{wide}");
}

/// What the by-value match does not build is refused by name: a move below a field (`W(P(s),
/// _)`) and a guard on a moving arm.
#[test]
fn a_match_by_value_refuses_what_it_does_not_build() {
    // CHANGED ON PURPOSE (walk/moves.rs): `W(P(s), _)` was refused ("a binding by value of a
    // part that needs drop, in a match by tests (a move out)") and is now built. A nested
    // variant pattern goes to the match by tests, which moves `G{5}` into `s` (dropped at the
    // arm's end) and leaves `G{6}`: a local's rest drops at its scope's end, after `k`
    // (declared later); a temporary's at its statement's end, before `k`.
    let deep = match_drops(Scrut::W, Arms::Deep, false).unwrap_or_else(|e| panic!("{e}"));
    let deep_temp = match_drops(Scrut::W, Arms::Deep, true).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(deep.as_slice(), &[5, 9, 6]);
        assert_eq!(deep_temp.as_slice(), &[5, 6, 9]);
    }
    let _ = (deep, deep_temp);
    // CHANGED ON PURPOSE (walk/moves.rs `Pending`): a guard on a moving arm was refused here
    // ("a guard on an arm that moves out of its scrutinee") and is now built:
    // `guarded_arms_move_only_after_the_guard_passes` below.
    // CHANGED ON PURPOSE (2026-11-16): a temporary scrutinee inside a larger expression was
    // refused here ("inside a larger expression") and is now built: tests/temp_drops.rs.
}

/// The Reference, "Match guards": "Moreover, by holding a shared reference while evaluating
/// the guard, mutation inside guards is also prevented"; a by-value binding of a guarded arm
/// moves its part only once the guard is true. A guard that passes moves as an unguarded arm
/// does; one that fails leaves the scrutinee whole for the next arm (`_`), and the binding's
/// copy is dropped by no one. Each case: the scrutinee, the arms, local or temporary, the
/// drops seen (`G`'s `x`; 9 is `k`). A move made before the guard (the bit cleared, the
/// binding not owned when the guard fails) loses a drop: `A, Guard(false)` gives `[9, 2]`.
#[test]
fn guarded_arms_move_only_after_the_guard_passes() {
    use Arms::*;
    use Scrut::*;
    let cases: &[(Scrut, Arms, bool, &[i32])] = &[
        // The switch (a local enum): `A(a, _) if g`.
        (A, Guard(true), false, &[1, 9, 2]),
        (A, Guard(false), false, &[9, 1, 2]),
        // The match by tests (a temporary): its rest at the statement's end.
        (A, Guard(true), true, &[1, 2, 9]),
        (A, Guard(false), true, &[1, 2, 9]),
        // A move below a field (`W(P(s), _) if g`), by tests, local and temporary.
        (W, DeepGuard(true), false, &[5, 9, 6]),
        (W, DeepGuard(false), false, &[9, 5, 6]),
        (W, DeepGuard(true), true, &[5, 6, 9]),
        (W, DeepGuard(false), true, &[5, 6, 9]),
    ];
    let mut failed = String::new();
    for &(s, arms, temp, want) in cases {
        match match_drops(s, arms, temp) {
            Ok(seen) if cfg!(not(target_arch = "aarch64")) || seen.as_slice() == want => {}
            Ok(seen) => failed.push_str(&format!("{s:?} {arms:?} temp={temp}: dropped {seen:?}, want {want:?}\n")),
            Err(e) => failed.push_str(&format!("{s:?} {arms:?} temp={temp}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
}

#[cfg(target_arch = "aarch64")]
mod objects;

#[cfg(target_arch = "aarch64")]
mod box_moves;

#[cfg(target_arch = "aarch64")]
mod closures;
/// A closure environment's layout alone, per place (stage 2 of captures as places).
mod capture_env;

mod forms;
mod projections;
mod places;
mod casts;
mod matches;
mod match_shapes;
mod guards;
mod promote;
mod const_paths;
mod target_intr;
#[cfg(target_arch = "aarch64")]
mod operators;
#[cfg(target_arch = "aarch64")]
mod arith_panics;
mod moves;
mod moves_places;
mod struct_update;
mod nounwind;
mod unwind;
mod unwind_ladder;
mod operand_cleanup_edges;
#[cfg(target_arch = "aarch64")]
mod temps;
#[cfg(target_arch = "aarch64")]
mod super_let;
#[cfg(target_arch = "aarch64")]
mod zst_drops;
#[cfg(target_arch = "aarch64")]
mod temp_drops;
#[cfg(target_arch = "aarch64")]
mod if_let_temps;
#[cfg(target_arch = "aarch64")]
mod discard;
mod lets;
#[cfg(target_arch = "aarch64")]
mod irrefutable;
#[cfg(target_arch = "aarch64")]
mod peeled_let;
#[cfg(target_arch = "aarch64")]
mod ref_bind;
#[cfg(target_arch = "aarch64")]
mod drop_patterns;
mod wide;
mod halfquad;
mod index;
mod fmt;
mod expr_shapes;
mod asm;
mod coroutines;
mod coroutine_held;
mod coroutine_id_agree;
#[cfg(target_arch = "aarch64")]
mod coroutines_emit;
mod never_reads;
mod track_caller;
