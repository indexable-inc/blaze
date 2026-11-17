//! `asm!`'s builtin form (`builtin # asm(..)`, what blaze-expand's `fn asm` writes for
//! `asm!`) lowered with its operands: one `Expr::Asm { row }` whose `AsmForm` row holds the
//! template's pieces, the operands, the options and the `clobber_abi` names.
//!
//! The rules, from the Rust Reference, "Inline assembly" (the pinned toolchain's
//! share/doc/rust/html/reference/inline-assembly.html):
//! - [asm.ts-args.syntax] "The assembler template uses the same syntax as format strings
//!   (i.e. placeholders are specified by curly braces)." and alloc's fmt docs:
//!   "maybe_format := '{' '{' | '}' '}' | format": `{{` and `}}` are one brace of text.
//! - "The corresponding arguments are accessed in order, by index, or by name."
//! - [asm.ts-args.one-or-more] "an asm! with multiple template string arguments is treated as
//!   if all the strings were concatenated with a \n between them": a `Line` piece between two
//!   templates.
//! - [asm.ts-args.positional-first] "As with format strings, positional arguments must appear
//!   before named arguments and explicit register operands."
//! - [asm.ts-args.register-operands] "Explicit register operands cannot be used by
//!   placeholders in the template string."
//! - [asm.ts-args.at-least-once] "All other named and positional operands must appear at least
//!   once in the template string, otherwise a compiler error is generated."
//! - [asm.ts-args.no-implicit] "However, implicit named arguments (introduced by RFC #2795) are
//!   not supported."
//! - [asm.template-modifiers.only-one] "Only one modifier is allowed per template placeholder."
//! - [asm.operand-type.supported-operands-sym] "`sym <path>`: <path> must refer to a fn or
//!   static." [asm.operand-type.supported-operands-const] "`const <expr>`: <expr> must be an
//!   integer constant expression."
//! - [asm.abi-clobbers.must-specify] "Generic register class outputs are disallowed by the
//!   compiler when clobber_abi is used: all outputs must specify an explicit register." An
//!   `inout`/`inlateout` operand is an output too ("`inout(<reg>) <in expr> => <out expr>`
//!   ... the output in <out expr>"); inputs may still name a class.
//! - Options: [asm.options.checks-mutually-exclusive] "The nomem and readonly options are
//!   mutually exclusive"; [asm.options.supported-options-pure] "The pure option must be
//!   combined with either the nomem or readonly options"; [asm.options.checks-pure] "It is a
//!   compile-time error to specify pure on an asm block with no outputs or only discarded
//!   outputs (_)"; [asm.options.checks-noreturn] "It is a compile-time error to specify
//!   noreturn on an asm block with outputs and without labels".
//! - [asm.scope.naked_asm] "The naked_asm! macro is only allowed in naked functions."
//!
//! Register classes and modifiers are the target's (the Reference's tables: AArch64 `reg` is
//! `x[0-30]` with the `w` modifier giving `w0`; x86-64 `xmm_reg`); the lowering keeps a class
//! by its name and checks none of them: every form here is valid on the target its text is
//! for. The real forms are zeroize's barrier (the prompt's `"/* {ptr} */"` form, and the text
//! zeroize 1.9.0's src/barrier.rs:69 has, `"# {}"` with a positional operand), cmov 0.5.4's
//! aarch64 `csel32!` (src/backends/aarch64.rs:5-19 and :109), and the Reference's own `sym`,
//! `clobber_abi` and `const` examples, written as the expander gives them back.

use blaze_lower::asm::{AsmForm, AsmKind, AsmPiece, AsmReg};
use blaze_lower::{Cfg, CfgAnswer, Edition, Expr, ExprId, NoModule, View, lower_body};

/// No cfg atom holds: these bodies read none.
struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::No
    }
}

const OPTION_NAMES: [(u16, &str); 9] = [
    (AsmForm::PURE, "pure"),
    (AsmForm::NOMEM, "nomem"),
    (AsmForm::READONLY, "readonly"),
    (AsmForm::PRESERVES_FLAGS, "preserves_flags"),
    (AsmForm::NORETURN, "noreturn"),
    (AsmForm::NOSTACK, "nostack"),
    (AsmForm::ATT_SYNTAX, "att_syntax"),
    (AsmForm::RAW, "raw"),
    (AsmForm::MAY_UNWIND, "may_unwind"),
];

fn kind_name(k: AsmKind) -> &'static str {
    match k {
        AsmKind::In => "in",
        AsmKind::Out => "out",
        AsmKind::LateOut => "lateout",
        AsmKind::InOut => "inout",
        AsmKind::InLateOut => "inlateout",
        AsmKind::Sym => "sym",
        AsmKind::Const => "const",
        AsmKind::Label => "label",
    }
}

fn expr_text<'b>(v: &View<'b>, e: ExprId) -> &'b str {
    let r = v.expr_range(e).expect("an operand's expression has its range");
    v.text_at(r).expect("an operand's range is of this record")
}

/// The body's one `asm!` printed as its row says, or what refused it.
fn lowered(text: &str) -> String {
    let alloc = blaze_own::testing::heap().alloc();
    let l = match lower_body(text, Edition::E2024, &mut NoCfg, &mut NoModule, alloc) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let v = l.view();
    let mut out = String::new();
    for d in v.diags() {
        let d = d.expect("a diagnostic row decodes");
        out.push_str(&format!("refused: {:?}\n", d.kind));
    }
    for u in v.unhandled() {
        out.push_str(&format!("unhandled: {:?}\n", u.why));
    }
    let asm = (0..v.exprs.len() as u32).find_map(|e| match v.expr(ExprId(e)) {
        Ok(Expr::Asm { row }) => Some(row),
        _ => None,
    });
    let Some(row) = asm else { return out };
    let form = v.asm_form(v.row(row).expect("the asm row")).expect("an AsmForm row");
    let mut options = String::new();
    for (bit, name) in OPTION_NAMES {
        if form.options & bit != 0 {
            if !options.is_empty() {
                options.push(',');
            }
            options.push_str(name);
        }
    }
    let mut pieces = String::new();
    for &n in v.rows(form.pieces).expect("the pieces") {
        if !pieces.is_empty() {
            pieces.push_str(", ");
        }
        match v.asm_piece(n).expect("a piece row") {
            AsmPiece::Text { text, .. } => {
                pieces.push_str(&format!("{:?}", v.text_at(text).expect("a text piece's range")));
            }
            AsmPiece::Operand { index, modifier: None, .. } => pieces.push_str(&format!("{{{index}}}")),
            AsmPiece::Operand { index, modifier: Some(m), .. } => pieces.push_str(&format!("{{{index}:{m}}}")),
            AsmPiece::Line => pieces.push('|'),
        }
    }
    let mut operands = String::new();
    for &n in v.rows(form.operands).expect("the operands") {
        if !operands.is_empty() {
            operands.push_str(", ");
        }
        let op = v.asm_operand(n).expect("an operand row");
        if let Some(name) = op.name {
            operands.push_str(&format!("{} = ", v.name_text(name).expect("a name")));
        }
        operands.push_str(kind_name(op.kind));
        match op.reg {
            Some(AsmReg::Class(c)) => operands.push_str(&format!("({})", v.name_text(c).expect("a class"))),
            Some(AsmReg::Explicit(r)) => operands.push_str(&format!("({:?})", v.name_text(r).expect("a register"))),
            None => {}
        }
        operands.push_str(&format!(" {}", expr_text(&v, op.expr)));
        if let Some(o) = op.out_expr {
            operands.push_str(&format!(" => {}", expr_text(&v, o)));
        }
    }
    let abis = v.ids(form.clobber_abis).expect("the abis").len();
    out.push_str(&format!("asm options={options} pieces=[{pieces}] operands=[{operands}] abis={abis}\n"));
    out
}

/// Every form here is one the Reference accepts; each lowers to its row. Prints
/// `asm options=readonly,preserves_flags,nostack pieces=["/* ", {0}, " */"]
/// operands=[ptr = in(reg) p] abis=0` first, then the other forms' lines, and asserts each.
#[test]
fn zeroize_and_cmov_forms() {
    let cases: [(&str, &str); 7] = [
        // zeroize's barrier as the prompt gives it.
        (
            r#"fn f(p: *const u8) { unsafe { builtin # asm("/* {ptr} */", ptr = in(reg) p, options(nostack, readonly, preserves_flags)) } }"#,
            "asm options=readonly,preserves_flags,nostack pieces=[\"/* \", {0}, \" */\"] operands=[ptr = in(reg) p] abis=0\n",
        ),
        // zeroize 1.9.0's src/barrier.rs:69.
        (
            r##"fn f<T: ?Sized>(val: &T) { unsafe { builtin # asm("# {}", in(reg) core::ptr::from_ref::<T>(val).cast::<()>(), options(readonly, preserves_flags, nostack)) } }"##,
            "asm options=readonly,preserves_flags,nostack pieces=[\"# \", {0}] operands=[in(reg) core::ptr::from_ref::<T>(val).cast::<()>()] abis=0\n",
        ),
        // cmov's aarch64 `csel32!`: two templates (one `Line` between), the `w` modifier of
        // AArch64's `reg` class, `pure` with `nomem` and a kept output (`inlateout ... *dst`).
        (
            r#"fn f(condition: u8, dst: &mut u8, src: &u8) { unsafe { builtin # asm("tst {0:w}, 0xff", "csel {1:w}, {2:w}, {3:w}, NE", in(reg) condition, inlateout(reg) *dst, in(reg) *src, in(reg) *dst, options(pure, nomem, nostack)) } }"#,
            "asm options=pure,nomem,nostack pieces=[\"tst \", {0:w}, \", 0xff\", |, \"csel \", {1:w}, \", \", {2:w}, \", \", {3:w}, \", NE\"] operands=[in(reg) condition, inlateout(reg) *dst, in(reg) *src, in(reg) *dst] abis=0\n",
        ),
        // `{{` and `}}` are one brace each; `{}` counts positional operands; an explicit
        // register needs no placeholder; `inout(..) a => b` keeps both places. No
        // `clobber_abi` here: with it the class output is refused (`refusals_name_their_rule`).
        (
            r#"fn f(a: u64, mut b: u64) { unsafe { builtin # asm("{{ {} }}", inout(reg) a => b, out("x0") _) } }"#,
            "asm options= pieces=[\"{\", \" \", {0}, \" }\"] operands=[inout(reg) a => b, out(\"x0\") _] abis=0\n",
        ),
        // The Reference's `clobber_abi` example ([asm.abi-clobbers]): a `sym` operand, the one
        // output in an explicit register, so `clobber_abi("C")` is allowed.
        (
            r#"fn f() -> i32 { extern "C" fn foo() -> i32 { 0 } let z: i32; unsafe { builtin # asm("call {}", sym foo, out("rax") z, clobber_abi("C")) }; z }"#,
            "asm options= pieces=[\"call \", {0}] operands=[sym foo, out(\"rax\") z] abis=1\n",
        ),
        // Inputs may name a class beside `clobber_abi`; the output is explicit (AArch64: `blr`
        // through a register, the result in `x0`).
        (
            r#"fn f(p: usize) -> u64 { let r: u64; unsafe { builtin # asm("blr {f}", f = in(reg) p, lateout("x0") r, clobber_abi("C")) }; r }"#,
            "asm options= pieces=[\"blr \", {0}] operands=[f = in(reg) p, lateout(\"x0\") r] abis=1\n",
        ),
        // The Reference's `const` example ([asm.operand-type.supported-operands-const]): one
        // named operand used twice, x86-64's `xmm_reg` class, `inlateout x => y`, a named
        // `const` (its constant written as the literal `SHUFFLE` holds).
        (
            r#"fn f(x: core::arch::x86_64::__m128) { let y: core::arch::x86_64::__m128; unsafe { builtin # asm("pshufd {xmm}, {xmm}, {shuffle}", xmm = inlateout(xmm_reg) x=>y, shuffle = const 0b01_00_10_11) } }"#,
            "asm options= pieces=[\"pshufd \", {0}, \", \", {0}, \", \", {1}] operands=[xmm = inlateout(xmm_reg) x => y, shuffle = const 0b01_00_10_11] abis=0\n",
        ),
    ];
    for (text, want) in cases {
        let got = lowered(text);
        println!("{got}");
        assert_eq!(got, want, "{text}");
    }
}

/// Each form the Reference forbids is refused naming the rule it breaks (one diagnostic, no
/// row); a form blaze does not lower yet is refused by name. Prints `refused:
/// Asm(NoSuchName)` first, then one line per case, and asserts each.
#[test]
fn refusals_name_their_rule() {
    let cases: [(&str, &str); 15] = [
        // [asm.ts-args.no-implicit]: `{x}` with no operand `x`.
        (r#"fn f() { unsafe { builtin # asm("/* {x} */") } }"#, "refused: Asm(NoSuchName)\n"),
        // A second `{}` with one positional operand.
        (r#"fn f(a: u64) { unsafe { builtin # asm("{} {}", in(reg) a) } }"#, "refused: Asm(NoSuchPositional)\n"),
        // [asm.ts-args.at-least-once].
        (r#"fn f(a: u64) { unsafe { builtin # asm("nop", in(reg) a) } }"#, "refused: Asm(UnusedOperand)\n"),
        // [asm.ts-args.register-operands]: an explicit register is no positional operand.
        (r#"fn f(a: u64) { unsafe { builtin # asm("{0}", in("x0") a) } }"#, "refused: Asm(NoSuchPositional)\n"),
        // [asm.ts-args.positional-first]: the Reference's two examples.
        (
            r#"fn f() { unsafe { builtin # asm("/* {x} {} */", x = const 5, in(reg) 5) } }"#,
            "refused: Asm(PositionalAfterNamed)\n",
        ),
        (
            r#"fn f() { unsafe { builtin # asm("/* {} */", in("eax") 0, in(reg) 5) } }"#,
            "refused: Asm(PositionalAfterNamed)\n",
        ),
        // [asm.template-modifiers.only-one].
        (r#"fn f(a: u64) { unsafe { builtin # asm("{0:ww}", in(reg) a) } }"#, "refused: Asm(ModifierNotOneChar)\n"),
        // [asm.abi-clobbers.must-specify]: `inout(reg) a => b` is a register-class output.
        (
            r#"fn f(a: u64, mut b: u64) { unsafe { builtin # asm("{{ {} }}", inout(reg) a => b, out("x0") _, clobber_abi("C")) } }"#,
            "refused: Asm(ClassOutputWithClobberAbi)\n",
        ),
        // The same rule on a plain `out(reg)`, the Reference's own example.
        (
            r#"fn f() -> i32 { extern "C" fn foo(x: i32) -> i32 { 0 } let z: i32; unsafe { builtin # asm("mov eax, {:e}", "call {}", out(reg) z, sym foo, clobber_abi("C")) }; z }"#,
            "refused: Asm(ClassOutputWithClobberAbi)\n",
        ),
        // [asm.options.checks-mutually-exclusive].
        (
            r#"fn f(a: u64) { unsafe { builtin # asm("{}", in(reg) a, options(nomem, readonly)) } }"#,
            "refused: Asm(NomemAndReadonly)\n",
        ),
        // [asm.options.supported-options-pure].
        (
            r#"fn f(mut x: u64) { unsafe { builtin # asm("mov {}, 1", out(reg) x, options(pure)) } }"#,
            "refused: Asm(PureNeedsNomemOrReadonly)\n",
        ),
        // [asm.options.checks-pure]: only a discarded output.
        (
            r#"fn f() { unsafe { builtin # asm("{}", out(reg) _, options(pure, nomem)) } }"#,
            "refused: Asm(PureWithoutOutputs)\n",
        ),
        // [asm.options.checks-noreturn].
        (
            r#"fn f(mut x: u64) { unsafe { builtin # asm("mov {}, 1", out(reg) x, options(noreturn)) } }"#,
            "refused: Asm(NoreturnWithOutputs)\n",
        ),
        // A valid template whose `{` is an escape (its value is `{0}`): blaze keeps pieces by
        // range in the literal's text, so it refuses the form by name.
        (r#"fn f(a: u64) { unsafe { builtin # asm("\u{7b}0}", in(reg) a) } }"#, "unhandled: AsmTemplateEscape\n"),
        // A naked function's `naked_asm!` (the Reference's codegen attributes chapter: "The
        // function body must consist of exactly one naked_asm! macro invocation"): not
        // lowered yet (INTRINSICS.md P12), refused by name.
        (r#"#[unsafe(naked)] extern "C" fn f() { builtin # naked_asm("ret") }"#, "unhandled: Asm\n"),
    ];
    for (text, want) in cases {
        let got = lowered(text);
        println!("{got}");
        assert_eq!(got, want, "{text}");
    }
}
