//! The named compare predicates: `cmpeqps`, `vcmpneq_oqpd`, `vcmpltsh` …
//!
//! `cmpps` and its relatives take the predicate as an immediate byte, and
//! every assembler also accepts a mnemonic with the predicate spelled into
//! it. Those are not separate instructions: the name simply supplies the
//! immediate, which is why they are derived here from the rows already in the
//! table rather than written out again.
//!
//! SSE has the eight predicates its immediate can hold. VEX widened the field
//! to five bits, so AVX has thirty-two, sixteen of which also keep the short
//! SSE-style spelling; `vcmpeqps` and `vcmpeq_oqps` are the same instruction.

use super::{Def, Op, Tbl, add};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The eight SSE predicates, in immediate order.
const SSE: [&str; 8] = ["eq", "lt", "le", "unord", "neq", "nlt", "nle", "ord"];

/// Every AVX predicate spelling and its immediate. The short names are the
/// SSE ones plus the six AVX added below 16; the long names name the
/// signalling behaviour as well, and cover all thirty-two.
#[rustfmt::skip]
const AVX: &[(&str, u8)] = &[
    ("eq", 0x00), ("eq_oq", 0x00), ("lt", 0x01), ("lt_os", 0x01),
    ("le", 0x02), ("le_os", 0x02), ("unord", 0x03), ("unord_q", 0x03),
    ("neq", 0x04), ("neq_uq", 0x04), ("nlt", 0x05), ("nlt_us", 0x05),
    ("nle", 0x06), ("nle_us", 0x06), ("ord", 0x07), ("ord_q", 0x07),
    ("eq_uq", 0x08), ("nge", 0x09), ("nge_us", 0x09), ("ngt", 0x0a),
    ("ngt_us", 0x0a), ("false", 0x0b), ("false_oq", 0x0b), ("neq_oq", 0x0c),
    ("ge", 0x0d), ("ge_os", 0x0d), ("gt", 0x0e), ("gt_os", 0x0e),
    ("true", 0x0f), ("true_uq", 0x0f), ("eq_os", 0x10), ("lt_oq", 0x11),
    ("le_oq", 0x12), ("unord_s", 0x13), ("neq_us", 0x14), ("nlt_uq", 0x15),
    ("nle_uq", 0x16), ("ord_s", 0x17), ("eq_us", 0x18), ("nge_uq", 0x19),
    ("ngt_uq", 0x1a), ("false_os", 0x1b), ("neq_os", 0x1c), ("ge_oq", 0x1d),
    ("gt_oq", 0x1e), ("true_us", 0x1f),
];

/// The rows of `mnem` that end in an `imm8`, with that operand dropped.
///
/// `cmpsd` names two instructions — the string compare and the scalar
/// floating-point compare — so the shape is what selects, not the name.
fn predicate_rows(t: &Tbl, mnem: &str) -> Vec<Def> {
    let Some(defs) = t.get(mnem) else {
        return Vec::new();
    };
    defs.iter()
        .filter(|d| d.ops.last() == Some(&Op::Imm(1)) && matches!(d.ops[0], Op::V(_)))
        .map(|d| {
            let mut d = d.clone();
            d.ops.pop();
            d
        })
        .collect()
}

fn install_family(t: &mut Tbl, stem: &str, flavours: &[&str], preds: &[(&str, u8)]) {
    for flav in flavours {
        let base = format!("{stem}{flav}");
        let rows = predicate_rows(t, &base);
        if rows.is_empty() {
            continue;
        }
        for &(name, imm) in preds {
            let alias = leak(format!("{stem}{name}{flav}"));
            let defs = rows.iter().map(|d| d.clone().suffix(imm)).collect();
            add(t, alias, defs);
        }
    }
}

/// The integer compares' predicates. Their immediate holds three bits, and
/// only six of the eight values have a name: `3` and `7` are always false and
/// always true, which nobody writes.
#[rustfmt::skip]
const INT: &[(&str, u8)] = &[
    ("eq", 0), ("lt", 1), ("le", 2), ("neq", 4), ("nlt", 5), ("nle", 6),
];

/// XOP's integer compares, which number the same predicates differently and
/// name all eight.
#[rustfmt::skip]
const XOP: &[(&str, u8)] = &[
    ("lt", 0), ("le", 1), ("gt", 2), ("ge", 3),
    ("eq", 4), ("neq", 5), ("false", 6), ("true", 7),
];

#[rustfmt::skip]
const PCLMUL: [(&str, u8); 4] = [
    ("lqlq", 0x00), ("hqlq", 0x01), ("lqhq", 0x10), ("hqhq", 0x11),
];

/// The element flavours the integer compares come in. The unsigned forms are
/// a different opcode, and carry their `u` before the element letter:
/// `vpcmpnequw`.
const INT_FLAVOURS: [&str; 8] = ["b", "w", "d", "q", "ub", "uw", "ud", "uq"];

pub fn install(t: &mut Tbl) {
    let sse: Vec<(&str, u8)> = SSE.iter().enumerate().map(|(i, n)| (*n, i as u8)).collect();
    install_family(t, "cmp", &["ps", "pd", "ss", "sd"], &sse);
    install_family(t, "vcmp", &["ps", "pd", "ss", "sd", "ph", "sh"], AVX);
    install_family(t, "vpcmp", &INT_FLAVOURS, INT);
    install_family(t, "vpcom", &INT_FLAVOURS, XOP);
    // The carry-less multiply names which quadword of each operand it takes:
    // `pclmullqhqdq` is the low one of the destination and the high one of
    // the source, immediate `0x10`.
    for stem in ["pclmul", "vpclmul"] {
        let base = format!("{stem}qdq");
        let rows = predicate_rows(t, &base);
        for (name, imm) in PCLMUL {
            let alias = leak(format!("{stem}{name}dq"));
            add(
                t,
                alias,
                rows.iter().map(|d| d.clone().suffix(imm)).collect(),
            );
        }
    }
}
