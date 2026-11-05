//! System registers, PSTATE fields and the named operands of the system
//! instructions.
//!
//! A system register is really five numbers — `op0:op1:CRn:CRm:op2` — and the
//! architecture keeps adding names for combinations of them; the same is true
//! of the `dc`, `ic`, `at` and `tlbi` operands, which are names for a `sys`
//! word. None of those names is written out here: `sysreg_data` holds them as
//! GNU as's own tables spell them, with the encoding each one assembles to.
//! The generic `S<op0>_<op1>_C<n>_C<m>_<op2>` spelling reaches a register
//! with no name at all, exactly as GNU as does.

use super::operand::Operand;
use super::sysreg_data as data;
use crate::arch::AsmCtx;
use crate::source::Span;

/// The `o0:op1:CRn:CRm:op2` bits of an `mrs`/`msr` word, already shifted
/// into place, and the restrictions the architecture puts on the register.
///
/// Which extension a register name belongs to is in GNU as's table and GNU
/// as 2.47 never reads it: its `sysreg_checking_p` starts at zero and
/// nothing turns it on, so every name it knows assembles whatever the
/// processor is. The named operands of the system instructions are gated;
/// see [`sys_ins`].
pub(crate) fn lookup(name: &str) -> Option<(u32, u8)> {
    match data::REGS.binary_search_by(|e| e.0.cmp(name)) {
        Ok(i) => Some((data::REGS[i].1, data::REGS[i].2)),
        Err(_) => generic(name).map(|bits| (bits, 0)),
    }
}

/// Parses a named system register.
#[allow(dead_code)]
pub fn by_name(name: &str) -> Option<u32> {
    lookup(name).map(|(bits, _)| bits)
}

/// The `S<op0>_<op1>_C<CRn>_C<CRm>_<op2>` escape hatch.
fn generic(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('s')?;
    let mut parts = rest.split('_');
    let op0: u32 = parts.next()?.parse().ok()?;
    let op1: u32 = parts.next()?.parse().ok()?;
    let crn: u32 = parts.next()?.strip_prefix('c')?.parse().ok()?;
    let crm: u32 = parts.next()?.strip_prefix('c')?.parse().ok()?;
    let op2: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // Only op0 2 and 3 are reachable: the opcode fixes the top two bits.
    if !(2..=3).contains(&op0) || op1 > 7 || crn > 15 || crm > 15 || op2 > 7 {
        return None;
    }
    Some(((op0 & 1) << 19) | (op1 << 16) | (crn << 12) | (crm << 8) | (op2 << 5))
}

/// Reads a system-register operand, reporting a diagnostic if it is not one.
#[allow(dead_code)]
pub fn operand(cx: &mut AsmCtx<'_>, op: &Operand<'_>) -> Option<u32> {
    register(cx, op, None)
}

/// Reads a system-register operand for `mrs` (`writing` false) or `msr`
/// (`writing` true), warning where GNU as warns: about a register the
/// architecture says cannot be read or written that way, and about one whose
/// name it may drop. Both assemblers still encode the word.
pub(crate) fn register(
    cx: &mut AsmCtx<'_>,
    op: &Operand<'_>,
    writing: Option<bool>,
) -> Option<u32> {
    let Some(n) = op.word() else {
        cx.error(op.span, "expected a system register name");
        return None;
    };
    let text = cx.name(n).to_ascii_lowercase();
    let Some((bits, flags)) = lookup(&text) else {
        cx.error(
            op.span,
            format!(
                "unknown system register `{text}`; \
                 write it as `s<op0>_<op1>_c<CRn>_c<CRm>_<op2>` if it has no name here"
            ),
        );
        return None;
    };
    let complaint = match writing {
        Some(true) if flags & data::READ_ONLY != 0 => Some("cannot be written to"),
        Some(false) if flags & data::WRITE_ONLY != 0 => Some("cannot be read from"),
        _ if flags & data::DEPRECATED != 0 => Some("is deprecated"),
        _ => None,
    };
    if let Some(what) = complaint {
        cx.diags
            .warning(op.span, format!("the system register `{text}` {what}"));
    }
    Some(bits)
}

/// A PSTATE field, which `msr` writes with an immediate rather than a
/// register: the word of `msr <field>, #0`, the bit the immediate starts at
/// and the largest value it may take.
pub(crate) fn pstate_field(name: &str) -> Option<(u32, u32, i64)> {
    data::PSTATE
        .iter()
        .find(|e| e.0 == name)
        .map(|&(_, word, lsb, max)| (word, u32::from(lsb), i64::from(max)))
}

/// The `op1`/`op2` pair of a PSTATE field.
#[allow(dead_code)]
pub fn pstate(name: &str) -> Option<(u32, u32)> {
    let (word, _, _) = pstate_field(name)?;
    Some(((word >> 16) & 7, (word >> 5) & 7))
}

/// `sys #0, c0, c0, #0, x0` and `sysl x0, #0, c0, c0, #0`: the words every
/// system instruction is written into, named operand or not.
pub(crate) const SYS: u32 = data::SYS;
pub(crate) const SYSL: u32 = data::SYSL;

/// The register a system instruction's named operand takes.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Xt {
    /// It takes none: `tlbi vmalle1is`.
    None,
    /// It must have one: `dc civac, x0`.
    Needs,
    /// It may have one: the TLB maintenance names that address a page.
    Optional,
}

/// A named operand of `dc`, `ic`, `at`, `tlbi` and their friends: the
/// `op1:CRn:CRm:op2` bits of the `sys` word it stands for.
///
/// GNU as refuses a name the selected processor does not have, and asks a
/// little less of it where the name may take an address register and none
/// was written: a TLB maintenance name without one is not a `tlbi` of the
/// TLBID extension. `with_register` says which it is.
pub(crate) fn sys_ins(
    cx: &mut AsmCtx<'_>,
    span: Span,
    mnemonic: &str,
    name: &str,
    with_register: bool,
) -> Option<(u32, Xt)> {
    let i = data::SYS_INS
        .binary_search_by(|e| (e.0, e.1).cmp(&(mnemonic, name)))
        .ok()?;
    let (_, _, bits, xt, with, without) = data::SYS_INS[i];
    let takes = match xt {
        data::NO_XT => Xt::None,
        data::NEEDS_XT => Xt::Needs,
        data::OPTIONAL_XT => Xt::Optional,
        _ => return None,
    };
    // An ARMv8-R core has no EL3, so none of the names that address it is
    // available there however the rest of the target reads.
    if name.ends_with("_el3") && super::cpu::supports(cx.state, super::cpu_data::V8R) {
        cx.error(
            span,
            format!("`{name}` addresses EL3, which this target has not"),
        );
        return None;
    }
    let want = data::FEATS[if with_register { with } else { without } as usize];
    if !super::cpu::supports(cx.state, want) {
        let msg = super::cpu::unsupported(cx.state, name, want);
        cx.error(span, format!("the system-instruction operand {msg}"));
        return None;
    }
    Some((bits, takes))
}

/// Whether this mnemonic takes one of those names at all.
pub(crate) fn is_sys_ins(mnemonic: &str) -> bool {
    data::SYS_INS.iter().any(|e| e.0 == mnemonic)
}

/// An alias of `hint` or of a barrier: a mnemonic on its own (`esb`, `sb`),
/// or one with a single named operand (`psb csync`, `dsb ishst`, `bti c`).
///
/// A diagnostic names the option rather than the mnemonic where there is
/// one, since it is the option that names the extension: `dsb ishnxs` and
/// its three neighbours are the `+xs` barriers and `dsb ish` is not.
pub(crate) fn hint(cx: &mut AsmCtx<'_>, span: Span, mnemonic: &str, option: &str) -> Option<u32> {
    let i = data::HINTS
        .binary_search_by(|e| (e.0, e.1).cmp(&(mnemonic, option)))
        .ok()?;
    let (_, _, word, feats) = data::HINTS[i];
    let want = data::FEATS[feats as usize];
    if !super::cpu::supports(cx.state, want) {
        let what = if option.is_empty() { mnemonic } else { option };
        let msg = super::cpu::unsupported(cx.state, what, want);
        cx.error(span, msg);
        return None;
    }
    Some(word)
}

/// Whether this mnemonic is one of those aliases.
pub(crate) fn is_hint(mnemonic: &str) -> bool {
    data::HINTS.iter().any(|e| e.0 == mnemonic)
}

/// Whether the pair is in the table at all, whatever the target has.
pub(crate) fn knows_hint(mnemonic: &str, option: &str) -> bool {
    data::HINTS
        .binary_search_by(|e| (e.0, e.1).cmp(&(mnemonic, option)))
        .is_ok()
}

/// The names this mnemonic accepts, for a diagnostic that lists them.
pub(crate) fn hint_options(mnemonic: &str) -> Vec<&'static str> {
    data::HINTS
        .iter()
        .filter(|e| e.0 == mnemonic && !e.1.is_empty())
        .map(|e| e.1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_generic_spellings_agree() {
        // TPIDRRO_EL0 is op0=3, op1=3, CRn=13, CRm=0, op2=3.
        assert_eq!(by_name("tpidrro_el0"), by_name("s3_3_c13_c0_3"));
        assert!(by_name("s3_3_c13_c0").is_none());
        assert!(by_name("s9_3_c13_c0_3").is_none());
        assert!(by_name("nosuchreg").is_none());
    }

    #[test]
    fn the_generated_tables_are_sorted_and_looked_up_by_name() {
        assert!(data::REGS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(
            data::SYS_INS
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1))
        );
        assert!(
            data::HINTS
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1))
        );
        // A name from each table, found the way the encoders find it.
        assert!(lookup("ctr_el0").is_some_and(|(_, f)| f & data::READ_ONLY != 0));
        // `dc civac` needs nothing of the target and `dc cgdsw` memory
        // tagging, which is the gate the last column carries.
        let feats = |m, n| {
            data::SYS_INS[data::SYS_INS
                .binary_search_by(|e| (e.0, e.1).cmp(&(m, n)))
                .expect("a name from the table")]
            .4
        };
        assert_eq!(data::FEATS[feats("dc", "civac") as usize], [0, 0, 0]);
        assert_ne!(data::FEATS[feats("dc", "cgdsw") as usize], [0, 0, 0]);
        assert!(knows_hint("psb", "csync"));
        assert!(!knows_hint("psb", "dsync"));
    }
}
