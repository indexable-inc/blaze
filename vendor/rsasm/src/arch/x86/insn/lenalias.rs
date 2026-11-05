//! The `x`, `y` and `z` spellings that name a vector length.
//!
//! A conversion that narrows, and `vfpclassps` and its relatives, leave the
//! width of a memory source undetermined: the destination is an `xmm` either
//! way, and in `vfpclass`'s case an opmask. Intel syntax says `xmmword ptr`;
//! AT&T, which has no size keyword, puts the length on the mnemonic instead,
//! so `vcvtpd2psx` reads 128 bits and `vcvtpd2psy` 256.
//!
//! These are the same instructions, so like the compare predicates they are
//! derived from the rows already in the table — here by keeping only the rows
//! of one length.
//!
//! GNU as takes `vfpclasspsx` in Intel syntax as well, where llvm-mc refuses
//! it; rsasm follows GNU as, as it does elsewhere on x86. The conversions are
//! AT&T-only in both.

use super::{ATT_ONLY, Def, Enc, Op, Tbl, add};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The conversions whose memory source can be either of two lengths. All are
/// AT&T-only spellings.
#[rustfmt::skip]
const CONVERSIONS: &[&str] = &[
    "vcvtpd2dq", "vcvtpd2ps", "vcvtpd2udq", "vcvtqq2ps", "vcvtuqq2ps",
    "vcvttpd2dq", "vcvttpd2udq", "vcvtneps2bf16",
];

/// `vfpclass`, where the length is on the mnemonic in both syntaxes.
const CLASSIFY: &[&str] = &["vfpclassps", "vfpclasspd"];

fn lengths_of(defs: &[Def], vlen: u16) -> Vec<Def> {
    defs.iter()
        .filter(|d| d.enc != Enc::Legacy && d.vlen == vlen)
        .cloned()
        .collect()
}

/// True if this row's memory operand is as wide as the vector, which is what
/// makes the plain mnemonic ambiguous in the first place.
fn narrowing(defs: &[Def]) -> bool {
    defs.iter()
        .any(|d| d.ops.iter().any(|o| matches!(o, Op::Vm(..))))
}

fn install_family(t: &mut Tbl, names: &[&str], att_only: bool, with_z: bool) {
    for &name in names {
        let Some(defs) = t.get(name).cloned() else {
            continue;
        };
        if !narrowing(&defs) {
            continue;
        }
        let mut which: Vec<(char, u16)> = vec![('x', 128), ('y', 256)];
        if with_z {
            which.push(('z', 512));
        }
        for (letter, vlen) in which {
            let rows = lengths_of(&defs, vlen);
            if rows.is_empty() {
                continue;
            }
            let alias = leak(format!("{name}{letter}"));
            let rows = rows
                .into_iter()
                .map(|d| if att_only { d.flags(ATT_ONLY) } else { d })
                .collect();
            add(t, alias, rows);
        }
    }
}

/// AVX-512FP16's narrowing conversions. A quadword source narrows to a
/// quarter of the register, so every length ends in an `xmm` and the 512-bit
/// form needs a `z` too.
#[rustfmt::skip]
const HALF_FROM_DWORD: &[&str] = &["vcvtdq2ph", "vcvtudq2ph", "vcvtps2phx"];
const HALF_FROM_QWORD: &[&str] = &["vcvtqq2ph", "vcvtuqq2ph", "vcvtpd2ph"];

pub fn install(t: &mut Tbl) {
    install_family(t, CONVERSIONS, true, false);
    install_family(t, HALF_FROM_DWORD, true, false);
    install_family(t, HALF_FROM_QWORD, true, true);
    install_family(t, CLASSIFY, false, true);
    // Unlike `vfpclassps`, `vfpclassph`'s lengths are AT&T spellings only.
    install_family(t, &["vfpclassph", "vfpclassbf16"], true, true);
    install_family(t, AVX10_NARROWING, true, false);
}

/// AVX10.2's conversions that narrow to half the register: the 8-bit floats
/// and the saturating double-to-dword truncations.
#[rustfmt::skip]
const AVX10_NARROWING: &[&str] = &[
    "vcvtph2bf8", "vcvtph2bf8s", "vcvtph2hf8", "vcvtph2hf8s",
    "vcvttpd2dqs", "vcvttpd2udqs",
];
