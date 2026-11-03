//! The x87 floating-point instruction set.
//!
//! Memory operands come in several widths that the operand size prefix has
//! nothing to do with: a `float` is 4 bytes, a `double` 8 and an extended
//! 10, while the integer forms load 2, 4 or 8. Intel syntax says which with a
//! size keyword. AT&T syntax says it with a suffix whose meaning depends on
//! the family — `s` is a 4-byte `flds` but a 2-byte `filds` — so each
//! suffixed spelling is a table entry of its own rather than something the
//! generic suffix rule could work out.

use super::{ATT_ONLY, Def, INTEL_ONLY, ModRm, NO66, Op, PLUSREG, Tbl, WAIT, add, d};

/// A memory form: the operand width, then the opcode and `/digit`.
type MemForm = (u8, u8, u8);

/// Rows for each memory form, smallest first. An unsized AT&T memory operand
/// takes the first, as GNU as does (it warns): `fld (%eax)` is `flds`, and
/// `fild (%eax)` is `filds`.
fn mem_rows(forms: &[MemForm]) -> Vec<Def> {
    forms
        .iter()
        .map(|&(w, op, ext)| d(vec![Op::M(w)], &[op], ModRm::Ext(ext), 0))
        .collect()
}

/// Installs `mnem` with every form, plus one entry per AT&T suffix that picks
/// a single memory width out of them.
fn family(
    t: &mut Tbl,
    mnem: &'static str,
    forms: &[MemForm],
    suffixes: &[(&str, u8)],
    extra: Vec<Def>,
) {
    let mut defs = mem_rows(forms);
    defs.extend(extra);
    add(t, mnem, defs);
    for &(suffix, w) in suffixes {
        let name: &'static str = Box::leak(format!("{mnem}{suffix}").into_boxed_str());
        let rows = forms
            .iter()
            .copied()
            .filter(|f| f.0 == w)
            .collect::<Vec<_>>();
        // Intel syntax says the width with a size keyword instead.
        let rows = mem_rows(&rows)
            .into_iter()
            .map(|r| r.flags(ATT_ONLY))
            .collect();
        add(t, name, rows);
    }
}

const FLOAT: &[(&str, u8)] = &[("s", 4), ("l", 8), ("t", 10)];
const INT: &[(&str, u8)] = &[("s", 2), ("l", 4), ("ll", 8), ("q", 8)];

fn st(opcode: [u8; 2]) -> Def {
    d(vec![Op::St], &opcode, ModRm::None, 0).flags(PLUSREG)
}

pub fn install(t: &mut Tbl) {
    // Operations with no operands.
    #[rustfmt::skip]
    let fixed: &[(&str, &[u8])] = &[
        ("fnop", &[0xd9, 0xd0]), ("fchs", &[0xd9, 0xe0]), ("fabs", &[0xd9, 0xe1]),
        ("ftst", &[0xd9, 0xe4]), ("fxam", &[0xd9, 0xe5]), ("fld1", &[0xd9, 0xe8]),
        ("fldl2t", &[0xd9, 0xe9]), ("fldl2e", &[0xd9, 0xea]), ("fldpi", &[0xd9, 0xeb]),
        ("fldlg2", &[0xd9, 0xec]), ("fldln2", &[0xd9, 0xed]), ("fldz", &[0xd9, 0xee]),
        ("f2xm1", &[0xd9, 0xf0]), ("fyl2x", &[0xd9, 0xf1]), ("fptan", &[0xd9, 0xf2]),
        ("fpatan", &[0xd9, 0xf3]), ("fxtract", &[0xd9, 0xf4]), ("fprem1", &[0xd9, 0xf5]),
        ("fdecstp", &[0xd9, 0xf6]), ("fincstp", &[0xd9, 0xf7]), ("fprem", &[0xd9, 0xf8]),
        ("fyl2xp1", &[0xd9, 0xf9]), ("fsqrt", &[0xd9, 0xfa]), ("fsincos", &[0xd9, 0xfb]),
        ("frndint", &[0xd9, 0xfc]), ("fscale", &[0xd9, 0xfd]), ("fsin", &[0xd9, 0xfe]),
        ("fcos", &[0xd9, 0xff]), ("fucompp", &[0xda, 0xe9]), ("fcompp", &[0xde, 0xd9]),
        ("fneni", &[0xdb, 0xe0]), ("fndisi", &[0xdb, 0xe1]), ("fnclex", &[0xdb, 0xe2]),
        ("fninit", &[0xdb, 0xe3]), ("fnsetpm", &[0xdb, 0xe4]), ("fwait", &[0x9b]),
    ];
    for &(mnem, bytes) in fixed {
        add(t, mnem, vec![d(vec![], bytes, ModRm::None, 0)]);
    }
    // The waiting forms of the control operations are `fwait` and the
    // non-waiting form.
    for (mnem, bytes) in [
        ("feni", [0xdb, 0xe0]),
        ("fdisi", [0xdb, 0xe1]),
        ("fclex", [0xdb, 0xe2]),
        ("finit", [0xdb, 0xe3]),
        ("fsetpm", [0xdb, 0xe4]),
    ] {
        add(t, mnem, vec![d(vec![], &bytes, ModRm::None, 0).flags(WAIT)]);
    }

    // Loads and stores.
    family(
        t,
        "fld",
        &[(4, 0xd9, 0), (8, 0xdd, 0), (10, 0xdb, 5)],
        FLOAT,
        vec![st([0xd9, 0xc0])],
    );
    family(
        t,
        "fst",
        &[(4, 0xd9, 2), (8, 0xdd, 2)],
        FLOAT,
        vec![st([0xdd, 0xd0])],
    );
    family(
        t,
        "fstp",
        &[(4, 0xd9, 3), (8, 0xdd, 3), (10, 0xdb, 7)],
        FLOAT,
        vec![st([0xdd, 0xd8])],
    );
    family(
        t,
        "fild",
        &[(2, 0xdf, 0), (4, 0xdb, 0), (8, 0xdf, 5)],
        INT,
        vec![],
    );
    family(t, "fist", &[(2, 0xdf, 2), (4, 0xdb, 2)], INT, vec![]);
    family(
        t,
        "fistp",
        &[(2, 0xdf, 3), (4, 0xdb, 3), (8, 0xdf, 7)],
        INT,
        vec![],
    );
    family(
        t,
        "fisttp",
        &[(2, 0xdf, 1), (4, 0xdb, 1), (8, 0xdd, 1)],
        INT,
        vec![],
    );
    add(t, "fbld", mem_rows(&[(10, 0xdf, 4)]));
    add(t, "fbstp", mem_rows(&[(10, 0xdf, 6)]));

    // Arithmetic. `D8 /digit` is `st(0) op= m32`, `DC /digit` the `m64`
    // form; with a register the same digit on `D8` is `st(0) op= st(i)`, and
    // on `DC` it is `st(i) op= st(0)`.
    for (mnem, digit) in [
        ("add", 0u8),
        ("mul", 1),
        ("sub", 4),
        ("subr", 5),
        ("div", 6),
        ("divr", 7),
    ] {
        let name: &'static str = Box::leak(format!("f{mnem}").into_boxed_str());
        let reg = 0xc0 + (digit << 3);
        // The manual pairs `DC E8+i` with `fsub st(i), st(0)` and `DC E0+i`
        // with `fsubr`, the reverse of the `D8` forms, and so on for the
        // divisions. AT&T syntax uses the `D8` pairing for both.
        let swapped = if digit >= 4 { reg ^ 0x08 } else { reg };
        let mut extra = vec![
            st([0xd8, reg]),
            d(vec![Op::Fixed("st"), Op::St], &[0xd8, reg], ModRm::None, 0).flags(PLUSREG),
            d(
                vec![Op::St, Op::Fixed("st")],
                &[0xdc, swapped],
                ModRm::None,
                0,
            )
            .flags(PLUSREG | INTEL_ONLY),
            d(vec![Op::St, Op::Fixed("st")], &[0xdc, reg], ModRm::None, 0)
                .flags(PLUSREG | ATT_ONLY),
        ];
        // With no operands AT&T means `st(1) op= st(0)`, popping.
        extra.push(d(vec![], &[0xde, reg + 1], ModRm::None, 0).flags(ATT_ONLY));
        family(t, name, &[(4, 0xd8, digit), (8, 0xdc, digit)], FLOAT, extra);

        let pname: &'static str = Box::leak(format!("f{mnem}p").into_boxed_str());
        add(
            t,
            pname,
            vec![
                d(
                    vec![Op::St, Op::Fixed("st")],
                    &[0xde, swapped],
                    ModRm::None,
                    0,
                )
                .flags(PLUSREG | INTEL_ONLY),
                d(vec![Op::St, Op::Fixed("st")], &[0xde, reg], ModRm::None, 0)
                    .flags(PLUSREG | ATT_ONLY),
                d(vec![Op::St], &[0xde, swapped], ModRm::None, 0).flags(PLUSREG | INTEL_ONLY),
                d(vec![Op::St], &[0xde, reg], ModRm::None, 0).flags(PLUSREG | ATT_ONLY),
                d(vec![], &[0xde, swapped + 1], ModRm::None, 0).flags(INTEL_ONLY),
                d(vec![], &[0xde, reg + 1], ModRm::None, 0).flags(ATT_ONLY),
            ],
        );

        let iname: &'static str = Box::leak(format!("fi{mnem}").into_boxed_str());
        family(t, iname, &[(2, 0xde, digit), (4, 0xda, digit)], INT, vec![]);
    }

    // Comparisons. With no operand they compare against `st(1)`.
    for (mnem, digit) in [("com", 2u8), ("comp", 3)] {
        let name: &'static str = Box::leak(format!("f{mnem}").into_boxed_str());
        let reg = 0xc0 + (digit << 3);
        family(
            t,
            name,
            &[(4, 0xd8, digit), (8, 0xdc, digit)],
            FLOAT,
            vec![st([0xd8, reg]), d(vec![], &[0xd8, reg + 1], ModRm::None, 0)],
        );
        let iname: &'static str = Box::leak(format!("fi{mnem}").into_boxed_str());
        family(t, iname, &[(2, 0xde, digit), (4, 0xda, digit)], INT, vec![]);
    }
    for (mnem, base) in [("fucom", [0xdd, 0xe0]), ("fucomp", [0xdd, 0xe8])] {
        add(
            t,
            mnem,
            vec![st(base), d(vec![], &[base[0], base[1] + 1], ModRm::None, 0)],
        );
    }
    for (mnem, base) in [
        ("fcomi", [0xdb, 0xf0]),
        ("fucomi", [0xdb, 0xe8]),
        ("fcomip", [0xdf, 0xf0]),
        ("fucomip", [0xdf, 0xe8]),
        ("fcmovb", [0xda, 0xc0]),
        ("fcmove", [0xda, 0xc8]),
        ("fcmovbe", [0xda, 0xd0]),
        ("fcmovu", [0xda, 0xd8]),
        ("fcmovnb", [0xdb, 0xc0]),
        ("fcmovne", [0xdb, 0xc8]),
        ("fcmovnbe", [0xdb, 0xd0]),
        ("fcmovnu", [0xdb, 0xd8]),
    ] {
        let mut defs = vec![d(vec![Op::Fixed("st"), Op::St], &base, ModRm::None, 0).flags(PLUSREG)];
        // The conditional moves always name both registers.
        if !mnem.starts_with("fcmov") {
            defs.push(st(base));
            defs.push(d(vec![], &[base[0], base[1] + 1], ModRm::None, 0));
        }
        add(t, mnem, defs);
    }
    add(
        t,
        "fxch",
        vec![st([0xd9, 0xc8]), d(vec![], &[0xd9, 0xc9], ModRm::None, 0)],
    );
    add(t, "ffree", vec![st([0xdd, 0xc0])]);
    add(t, "ffreep", vec![st([0xdf, 0xc0])]);

    // Control and environment.
    for (mnem, op, ext, width, wait) in [
        ("fldcw", 0xd9u8, 5u8, 2u8, false),
        ("fnstcw", 0xd9, 7, 2, false),
        ("fstcw", 0xd9, 7, 2, true),
        ("fnstsw", 0xdd, 7, 2, false),
        ("fstsw", 0xdd, 7, 2, true),
        ("fldenv", 0xd9, 4, 0, false),
        ("fnstenv", 0xd9, 6, 0, false),
        ("fstenv", 0xd9, 6, 0, true),
        ("frstor", 0xdd, 4, 0, false),
        ("fnsave", 0xdd, 6, 0, false),
        ("fsave", 0xdd, 6, 0, true),
    ] {
        let flags = if wait { WAIT } else { 0 };
        let mut defs = vec![d(vec![Op::M(width)], &[op], ModRm::Ext(ext), 0).flags(flags)];
        // The word forms may be written with the `w` suffix, which asks for
        // no prefix: `fnstcww`.
        if width == 2 {
            defs.push(d(vec![Op::M(2)], &[op], ModRm::Ext(ext), 16).flags(flags | NO66 | ATT_ONLY));
        }
        if mnem.ends_with("stsw") {
            defs.push(d(vec![Op::Fixed("ax")], &[0xdf, 0xe0], ModRm::None, 0).flags(flags));
        }
        add(t, mnem, defs);
    }
}
