//! `format_args!` (walk/fmt.rs), built from a body the checker types against STAND-INS of the
//! pinned library's `core::fmt::Arguments` and `rt::Argument` of the same shape, then run
//! natively.
//!
//! The pinned library (library/core/src/fmt/mod.rs, "Internal representation", and rt.rs):
//! `Arguments { template: NonNull<u8>, args: NonNull<rt::Argument<'a>> }`; the template's
//! bytes are each literal piece as its length byte and bytes, a placeholder of default options
//! as `0xC0` ("the next argument in the `args` array"), and a final `0`; `args` points at an
//! array of `rt::Argument`, each made by `Argument::new_display::<T>(x: &T)` (or the
//! constructor of the placeholder's trait) from a reference to the argument.
//!
//! What the stand-ins leave out, which the real library needs from the world beyond this test:
//! `Arguments`' fields are `NonNull` (here raw pointers of the same layout); `rt::Argument` is
//! `ArgumentType`'s enum (a value pointer and a formatter fn pointer, made by
//! `argument_new!`'s transmute of `<T as Display>::fmt`), here a struct of the address and a
//! code naming the constructor; the real constructors are `const fn`s in `impl Argument<'_>`
//! whose instance at `[T]` the world must answer through `Adts::fmt_argument_new`, and
//! `Adts::format_template` must come from blaze-lower's `FormatArgs` row (its `FmtPiece::Text`
//! texts and `FmtPiece::Arg` specs), which the checker's body form does not keep.

use super::*;

/// The stand-in `core::fmt::Arguments` (lang `format_arguments`).
const ARGUMENTS: ItemKey = ItemKey(60);
/// The stand-in `core::fmt::rt::Argument` (lang `format_argument`).
const ARGUMENT: ItemKey = ItemKey(61);
/// The stand-in `rt::Argument::new_display`.
const NEW_DISPLAY: ItemKey = ItemKey(62);
/// The trait `Display` (lang `FmtDisplay`) and its impl for `u32`.
const DISPLAY: ItemKey = ItemKey(63);
const DISPLAY_U32: ItemKey = ItemKey(64);
const TRANSMUTE: ItemKey = ItemKey(65);
/// The stand-in `rt::Argument::from_usize` (`fn from_usize(x: &usize) -> Argument`).
const FROM_USIZE: ItemKey = ItemKey(66);

const U64: PNode = PNode::Uint(UintTy::U64);

/// `TestWorld` with `Display`, `impl Display for u32` and the `format_args!` lang items.
struct FmtWorld {
    w: TestWorld,
    u32_: PTy,
}

impl World for FmtWorld {
    fn fn_sig(&self, s: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        self.w.fn_sig(s, item)
    }

    fn closure(&self, s: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<blaze_typeck::ClosureFacts<'_>>> {
        World::closure(&self.w, s, owner, ordinal)
    }

    fn predicates(&self, s: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        self.w.predicates(s, item)
    }

    fn adt(&self, s: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        World::adt(&self.w, s, item)
    }

    fn impls_of(&self, s: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        if trait_ == DISPLAY {
            return Ok(if matches!(shape, Shape::Uint(UintTy::U32)) { &[DISPLAY_U32] } else { &[] });
        }
        self.w.impls_of(s, trait_, shape)
    }

    fn impl_header(&self, s: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        if item == DISPLAY_U32 {
            return Ok(Some(ImplHeader {
                generics: 0,
                self_ty: self.u32_,
                trait_: Some(DISPLAY),
                trait_args: blaze_typeck::ArgsAfterSelf::NONE,
                assoc_types: &[],
                types: self.w.types.view(),
                polarity: blaze_typeck::ImplPolarity::Positive,
            }));
        }
        self.w.impl_header(s, item)
    }

    fn inherent(&self, s: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        self.w.inherent(s, shape, name)
    }

    fn assoc_item(&self, s: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        self.w.assoc_item(s, container, name)
    }

    fn traits_in_scope(&self, s: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        self.w.traits_in_scope(s, scope)
    }

    fn resolve_path(&self, s: Seal, scope: ScopeId, path: PathIdx, segments: &[Name]) -> Fetch<blaze_typeck::PathAnswer> {
        self.w.resolve_path(s, scope, path, segments)
    }

    fn lang_item(&self, s: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        match item {
            LangItem::FormatArguments => Ok(Some(ARGUMENTS)),
            LangItem::FmtDisplay => Ok(Some(DISPLAY)),
            _ => self.w.lang_item(s, item),
        }
    }

    fn lang_path(&self, s: Seal, item: LangItem) -> Fetch<Option<Def>> {
        self.w.lang_path(s, item)
    }

    fn const_value(&self, s: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.w.const_value(s, item)
    }

    fn opaque_hidden(&self, s: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        World::opaque_hidden(&self.w, s, item)
    }

    fn attrs(&self, s: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        self.w.attrs(s, item)
    }

    fn trait_def(&self, s: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::TraitDef<'_>>> {
        if item == DISPLAY {
            return Ok(Some(blaze_typeck::TraitDef {
                generics: 1,
                flags: blaze_typeck::TraitFlags(0),
                lang: Some(LangItem::FmtDisplay),
                supertraits: &[],
                members: blaze_typeck::TraitMembers::NotKnown,
                assoc_types: &[],
                types: self.w.types.view(),
                defaults: blaze_typeck::TraitDefaults::Known(&[]),
            }));
        }
        self.w.trait_def(s, item)
    }

    fn assoc_parent(&self, s: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::AssocParent>> {
        self.w.assoc_parent(s, item)
    }

    fn const_holds(&self, s: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<blaze_typeck::ConstHolds> {
        self.w.const_holds(s, cond, args, types)
    }

    fn const_params(&self, s: Seal, item: ItemKey) -> Fetch<blaze_typeck::ConstParams<'_>> {
        self.w.const_params(s, item)
    }
}

/// The test world's answers, and the three `format_args!` hooks: the template `"a{}b"`.
struct FmtAdts<'a> {
    inner: TestAdts<'a>,
    template: &'a [crate::FmtPiece<'a>],
}

impl Adts for FmtAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.inner.adt(key)
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        self.inner.overflow_checks()
    }

    fn ub_checks(&self) -> crate::UbChecks {
        self.inner.ub_checks()
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self.inner.facts()
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        self.inner.drop_impl(ty, ground)
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.inner.int_lit(owner, expr)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.inner.intrinsic(item)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.inner.abi(item)
    }

    /// The test indexes the template's bytes: `panic_bounds_check`.
    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        self.inner.lang_fn(name)
    }

    fn format_template(&self, _: ItemKey, _: ExprId) -> Option<&[crate::FmtPiece<'_>]> {
        Some(self.template)
    }

    fn fmt_argument_new(&self, trait_: blaze_typeck::FmtTrait) -> Option<ItemKey> {
        match trait_ {
            blaze_typeck::FmtTrait::Display => Some(NEW_DISPLAY),
            blaze_typeck::FmtTrait::Count => Some(FROM_USIZE),
            _ => None,
        }
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        Some(ARGUMENT)
    }
}

/// The pinned template encoding (module doc): `"a{}b"` is `[1, b'a', 0xC0, 1, b'b', 0]`.
#[test]
fn the_template_is_encoded_as_the_pinned_library_reads_it() {
    let mut out = blaze_own::Vec::new_in(Global);
    crate::walk::fmt_encode_text_for_tests("a", &mut out);
    out.push(0xC0);
    crate::walk::fmt_encode_text_for_tests("b", &mut out);
    crate::walk::fmt_encode_text_for_tests("", &mut out);
    out.push(0);
    assert_eq!(out.as_slice(), &[1, b'a', 0xC0, 1, b'b', 0]);
    // 200 bytes: past the one-byte form, `0x80` and a little-endian `u16` length.
    let long = "x".repeat(200);
    let mut out = blaze_own::Vec::new_in(Global);
    crate::walk::fmt_encode_text_for_tests(&long, &mut out);
    assert_eq!(&out[..3], &[0x80, 200, 0]);
    assert_eq!(out.len(), 203);
}

/// ```text
/// struct Arguments { template: *const u8, args: *const Argument }   // lang format_arguments
/// struct Argument { value: usize, code: u64 }                        // lang format_argument
/// fn new_display<T>(x: &T) -> Argument { Argument { value: transmute(x), code: 1 } }
/// fn main() -> u64 {
///     let x = 5u32;
///     let a = format_args!("a{}b", x);
///     let t: &[u8; 6] = transmute(a.template);
///     let g: &Argument = transmute(a.args);
///     let v: usize = transmute(&x);
///     (((*g).value == v) as u64) * 1_000_000     // the argument's own address, borrowed in place
///         + (*g).code * 100_000                  // `new_display` was the constructor called
///         + ((*t)[0] as u64) * 10_000            // the first piece's length byte: 1 ("a")
///         + ((*t)[3] as u64) * 1000              // the second piece's length byte: 1 ("b")
///         + (*t)[2] as u64                       // the one placeholder: 0xC0 = 192
/// }
/// ```
/// 1_111_192. A build that copies `x` (address differs), calls no constructor, or lays the
/// pieces out as a `&[&str]` fails it.
#[test]
fn format_args_builds_the_pinned_arguments_from_one_display_argument() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U64);
    w.bounds_fn();
    let u8_ = w.ty(U8);
    let arg_ty = w.ty(PNode::Adt(ARGUMENT, List::EMPTY));
    w.strukt(ARGUMENTS.0, "Arguments", &[("template", PNode::Ptr(Mutbl::Not, u8_)), ("args", PNode::Ptr(Mutbl::Not, arg_ty))]);
    w.strukt(ARGUMENT.0, "Argument", &[("value", USIZE), ("code", U64)]);
    let t_param = w.ty(PNode::Param(0));
    w.generic_func(NEW_DISPLAY.0, "new_display", 1, &[PNode::Ref(Mutbl::Not, t_param)], PNode::Adt(ARGUMENT, List::EMPTY));
    w.generic_func(TRANSMUTE.0, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((TRANSMUTE, Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let u32_ = w.ty(U32);
    let world = FmtWorld { w, u32_ };

    // main.
    let mut b = BodyBuf::new();
    let u8_b = b.types.intern(U8).expect("a type interns");
    let u64_b = b.types.intern(U64).expect("a type interns");
    let usize_b = b.types.intern(USIZE).expect("a type interns");
    let six = b.types.usize_value(6).expect("a length interns");
    let arr = b.types.intern(PNode::Array(u8_b, six)).expect("a type interns");
    let arr_ref = b.types.intern(PNode::Ref(Mutbl::Not, arr)).expect("a type interns");
    let argument_b = b.types.intern(PNode::Adt(ARGUMENT, List::EMPTY)).expect("a type interns");
    let arg_ref = b.types.intern(PNode::Ref(Mutbl::Not, argument_b)).expect("a type interns");
    let (x, a, t, g, v) = (b.bind("x"), b.bind("a"), b.bind("t"), b.bind("g"), b.bind("v"));
    let x_init = b.int(5, Some(IntSuffix::Uint(UintTy::U32)));
    let x_arg = b.local(x);
    let fargs = b.exprs_list(&[x_arg]);
    let uses = [blaze_typeck::FmtUse { arg: 0, trait_: blaze_typeck::FmtTrait::Display }];
    let a_init = b.e(Expr::FormatArgs { args: fargs, uses: List { start: 0, len: 1 } });
    let a_e = b.local(a);
    let tmpl = b.field(a_e, "template");
    let t_init = b.call("transmute", &[tmpl]);
    let a_e = b.local(a);
    let argsp = b.field(a_e, "args");
    let g_init = b.call("transmute", &[argsp]);
    let x_e = b.local(x);
    let x_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: x_e });
    let v_init = b.call("transmute", &[x_ref]);
    let deref = |b: &mut BodyBuf, p: PatId| {
        let e = b.local(p);
        b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e })
    };
    let as_u64 = |b: &mut BodyBuf, e: ExprId| b.e(Expr::Cast { operand: e, ty: u64_b });
    let byte = |b: &mut BodyBuf, i: u128| {
        let base = deref(b, t);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        b.e(Expr::Cast { operand: at, ty: u64_b })
    };
    let gd = deref(&mut b, g);
    let value = b.field(gd, "value");
    let v_e = b.local(v);
    let same = b.bin(BinOp::Eq, value, v_e);
    let same = as_u64(&mut b, same);
    let k = b.int(1_000_000, None);
    let s0 = b.bin(BinOp::Mul, same, k);
    let gd = deref(&mut b, g);
    let code = b.field(gd, "code");
    let k = b.int(100_000, None);
    let s1 = b.bin(BinOp::Mul, code, k);
    let b0 = byte(&mut b, 0);
    let k = b.int(10_000, None);
    let s2 = b.bin(BinOp::Mul, b0, k);
    let b3 = byte(&mut b, 3);
    let k = b.int(1000, None);
    let s3 = b.bin(BinOp::Mul, b3, k);
    let s4 = byte(&mut b, 2);
    let sum = b.bin(BinOp::Add, s0, s1);
    let sum = b.bin(BinOp::Add, sum, s2);
    let sum = b.bin(BinOp::Add, sum, s3);
    let sum = b.bin(BinOp::Add, sum, s4);
    let root = b.block(
        &[
            Stmt::Let { pat: x, ty: None, init: Some(x_init), else_: None },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: t, ty: Some(arr_ref), init: Some(t_init), else_: None },
            Stmt::Let { pat: g, ty: Some(arg_ref), init: Some(g_init), else_: None },
            Stmt::Let { pat: v, ty: Some(usize_b), init: Some(v_init), else_: None },
        ],
        Some(sum),
    );
    let _ = u8_;

    let template = [crate::FmtPiece::Text("a"), crate::FmtPiece::Arg { spec: "" }, crate::FmtPiece::Text("b")];
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_fmt(&world, &b, &uses, &template, root, &mut ground).unwrap_or_else(|e| panic!("{e}"));
    assert!(main.refs.iter().any(|r| r.item == NEW_DISPLAY), "the constructor is called: {:?}", main.refs);

    // new_display<T>, at [u32].
    let mut c = BodyBuf::new();
    let xp = c.bind("x");
    c.params.push(xp);
    let path = c.path("Argument");
    let x_e = c.local(xp);
    let value = c.call("transmute", &[x_e]);
    let one = c.int(1, None);
    let start = c.field_inits.len() as u32;
    c.field_inits.push(FieldInit { name: Name::of("value"), expr: value });
    c.field_inits.push(FieldInit { name: Name::of("code"), expr: one });
    let lit = c.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let croot = c.block(&[], Some(lit));
    let u32_g = ground.intern(U32).expect("a type interns");
    let ctor = build_instance(&world.w, &c, NEW_DISPLAY, croot, &[u32_g], &mut ground).unwrap_or_else(|e| panic!("{e}"));

    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(NEW_DISPLAY, None, &ctor), (MAIN, None, &main)]), 1_111_192);
    }
    let _ = ctor;
}

/// A width read from an argument (std::fmt, "Width": `{:1$}` takes the width from argument 1,
/// a `usize`):
/// ```text
/// fn from_usize(x: &usize) -> Argument { Argument { value: transmute(x), code: 2 } }
/// fn main() -> u64 {
///     let x = 5u32;
///     let w = 7usize;
///     let a = format_args!("{:1$}", x, w);
///     let t: &[u8; 8] = transmute(a.template);
///     let g: &[Argument; 2] = transmute(a.args);
///     let v: usize = transmute(&w);
///     (((*g)[1].value == v) as u64) * 1_000_000   // the count's entry borrows `w` in place
///         + (*g)[0].code * 100_000                 // entry 0: the value's `new_display`
///         + (*g)[1].code * 10_000                  // entry 1: the count's `from_usize`
///         + ((*t)[5] as u64) * 1000                // the width field: entry 1
///         + (*t)[0] as u64                         // the head: 0xD3 = 211 (flags, width, indirect)
/// }
/// ```
/// 1_121_211. The template is `fmt::write`'s (module doc of walk/fmt.rs): head `0xC0 | 1 | 2 |
/// 16`, flags `0x6800_0020` (fill ' ', width set, alignment unset), width field 1. A build that
/// refuses the count, puts the count first (the value would format entry 0, a `Count`), or
/// writes the width without its indirect bit fails it.
#[test]
fn format_args_reads_a_width_from_an_argument() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U64);
    w.bounds_fn();
    let u8_ = w.ty(U8);
    let arg_ty = w.ty(PNode::Adt(ARGUMENT, List::EMPTY));
    w.strukt(ARGUMENTS.0, "Arguments", &[("template", PNode::Ptr(Mutbl::Not, u8_)), ("args", PNode::Ptr(Mutbl::Not, arg_ty))]);
    w.strukt(ARGUMENT.0, "Argument", &[("value", USIZE), ("code", U64)]);
    let t_param = w.ty(PNode::Param(0));
    w.generic_func(NEW_DISPLAY.0, "new_display", 1, &[PNode::Ref(Mutbl::Not, t_param)], PNode::Adt(ARGUMENT, List::EMPTY));
    let usize_w = w.ty(USIZE);
    w.func(FROM_USIZE.0, "from_usize", &[PNode::Ref(Mutbl::Not, usize_w)], PNode::Adt(ARGUMENT, List::EMPTY));
    w.generic_func(TRANSMUTE.0, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((TRANSMUTE, Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let u32_ = w.ty(U32);
    let world = FmtWorld { w, u32_ };

    let mut b = BodyBuf::new();
    let u8_b = b.types.intern(U8).expect("a type interns");
    let u64_b = b.types.intern(U64).expect("a type interns");
    let usize_b = b.types.intern(USIZE).expect("a type interns");
    let eight = b.types.usize_value(8).expect("a length interns");
    let arr = b.types.intern(PNode::Array(u8_b, eight)).expect("a type interns");
    let arr_ref = b.types.intern(PNode::Ref(Mutbl::Not, arr)).expect("a type interns");
    let argument_b = b.types.intern(PNode::Adt(ARGUMENT, List::EMPTY)).expect("a type interns");
    let two = b.types.usize_value(2).expect("a length interns");
    let args_arr = b.types.intern(PNode::Array(argument_b, two)).expect("a type interns");
    let args_ref = b.types.intern(PNode::Ref(Mutbl::Not, args_arr)).expect("a type interns");
    let (x, wd, a, t, g, v) = (b.bind("x"), b.bind("w"), b.bind("a"), b.bind("t"), b.bind("g"), b.bind("v"));
    let x_init = b.int(5, Some(IntSuffix::Uint(UintTy::U32)));
    let w_init = b.int(7, Some(IntSuffix::Uint(UintTy::Usize)));
    let x_arg = b.local(x);
    let w_arg = b.local(wd);
    let fargs = b.exprs_list(&[x_arg, w_arg]);
    let uses = [
        blaze_typeck::FmtUse { arg: 1, trait_: blaze_typeck::FmtTrait::Count },
        blaze_typeck::FmtUse { arg: 0, trait_: blaze_typeck::FmtTrait::Display },
    ];
    let a_init = b.e(Expr::FormatArgs { args: fargs, uses: List { start: 0, len: 2 } });
    let a_e = b.local(a);
    let tmpl = b.field(a_e, "template");
    let t_init = b.call("transmute", &[tmpl]);
    let a_e = b.local(a);
    let argsp = b.field(a_e, "args");
    let g_init = b.call("transmute", &[argsp]);
    let w_e = b.local(wd);
    let w_ref = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: w_e });
    let v_init = b.call("transmute", &[w_ref]);
    let deref = |b: &mut BodyBuf, p: PatId| {
        let e = b.local(p);
        b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e })
    };
    let entry = |b: &mut BodyBuf, i: u128, name: &str| {
        let base = deref(b, g);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        b.field(at, name)
    };
    let byte = |b: &mut BodyBuf, i: u128| {
        let base = deref(b, t);
        let ix = b.int(i, None);
        let at = b.index(base, ix);
        b.e(Expr::Cast { operand: at, ty: u64_b })
    };
    let value = entry(&mut b, 1, "value");
    let v_e = b.local(v);
    let same = b.bin(BinOp::Eq, value, v_e);
    let same = b.e(Expr::Cast { operand: same, ty: u64_b });
    let k = b.int(1_000_000, None);
    let s0 = b.bin(BinOp::Mul, same, k);
    let code0 = entry(&mut b, 0, "code");
    let k = b.int(100_000, None);
    let s1 = b.bin(BinOp::Mul, code0, k);
    let code1 = entry(&mut b, 1, "code");
    let k = b.int(10_000, None);
    let s2 = b.bin(BinOp::Mul, code1, k);
    let b5 = byte(&mut b, 5);
    let k = b.int(1000, None);
    let s3 = b.bin(BinOp::Mul, b5, k);
    let s4 = byte(&mut b, 0);
    let sum = b.bin(BinOp::Add, s0, s1);
    let sum = b.bin(BinOp::Add, sum, s2);
    let sum = b.bin(BinOp::Add, sum, s3);
    let sum = b.bin(BinOp::Add, sum, s4);
    let root = b.block(
        &[
            Stmt::Let { pat: x, ty: None, init: Some(x_init), else_: None },
            Stmt::Let { pat: wd, ty: None, init: Some(w_init), else_: None },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: t, ty: Some(arr_ref), init: Some(t_init), else_: None },
            Stmt::Let { pat: g, ty: Some(args_ref), init: Some(g_init), else_: None },
            Stmt::Let { pat: v, ty: Some(usize_b), init: Some(v_init), else_: None },
        ],
        Some(sum),
    );

    let template = [crate::FmtPiece::Arg { spec: "1$" }];
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_fmt(&world, &b, &uses, &template, root, &mut ground).unwrap_or_else(|e| panic!("{e}"));
    assert!(main.refs.iter().any(|r| r.item == FROM_USIZE), "the count's constructor is called: {:?}", main.refs);

    // new_display<T> at [u32], code 1; from_usize, code 2.
    let ctor = |code: u128| {
        let mut c = BodyBuf::new();
        let xp = c.bind("x");
        c.params.push(xp);
        let path = c.path("Argument");
        let x_e = c.local(xp);
        let value = c.call("transmute", &[x_e]);
        let k = c.int(code, None);
        let start = c.field_inits.len() as u32;
        c.field_inits.push(FieldInit { name: Name::of("value"), expr: value });
        c.field_inits.push(FieldInit { name: Name::of("code"), expr: k });
        let lit = c.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
        let croot = c.block(&[], Some(lit));
        (c, croot)
    };
    let (dc, droot) = ctor(1);
    let u32_g = ground.intern(U32).expect("a type interns");
    let display = build_instance(&world.w, &dc, NEW_DISPLAY, droot, &[u32_g], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let (uc, uroot) = ctor(2);
    let count = build_instance(&world.w, &uc, FROM_USIZE, uroot, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));

    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(
            run_native_with(&host, &[(NEW_DISPLAY, None, &display), (FROM_USIZE, None, &count), (MAIN, None, &main)]),
            1_121_211
        );
    }
    let _ = (display, count);
}

/// `build_instance` for `MAIN` against [`FmtWorld`] and [`FmtAdts`], with the body's
/// `fmt_uses`, which `BodyBuf::view` leaves empty.
fn build_fmt(
    world: &FmtWorld,
    body: &BodyBuf,
    uses: &[blaze_typeck::FmtUse],
    template: &[crate::FmtPiece<'_>],
    root: ExprId,
    ground: &mut PortableTys<'static>,
) -> Result<Built<Global>, String> {
    let mut view = body.view(MAIN, root);
    view.fmt_uses = uses;
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, world, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => return Err(format!("the check needed {:?}", n.questions)),
        Err(error) => return Err(format!("the check failed: {error:?}")),
    };
    if !checked.diags.is_empty() {
        return Err(format!("diagnostics: {:?}", checked.diags));
    }
    let sig = world.w.sig(MAIN).ok_or_else(|| String::from("no signature for main"))?;
    let mut subst = Subst { args: &[], ground, scratch: Alloc::global() };
    let adts = FmtAdts { inner: TestAdts { world: &world.w, body }, template };
    let built = crate::build(&view, &checked, &sig, &mut subst, &adts, Global).map_err(|error| format!("the build failed: {error}"))?;
    blaze_lir::verify(built.func.view()).map_err(|error| format!("does not verify: {error}"))?;
    emits(&built, &[]);
    Ok(built)
}
