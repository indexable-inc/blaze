//! The sections a MIPS assembler writes into every object of its own
//! accord: `.reginfo` or `.MIPS.options`, which say which registers the code
//! touches, and `.MIPS.abiflags`, which says what it needs of a processor.
//!
//! llvm-mc is the reference for MIPS (see the README's Verification
//! section), and `tools/mc-diff` compares these sections along with the rest
//! of the object. GNU as writes `.MIPS.abiflags` identically, and a
//! `.reginfo` whose `sh_flags` leave out `SHF_ALLOC` and whose masks it
//! works out almost the same way; the one place the two differ is below. It
//! also writes an empty `.pdr` and a `.gnu.attributes` recording the
//! floating-point ABI, neither of which llvm-mc nor rsasm writes.
//!
//! # The register masks
//!
//! `ri_gprmask` and `ri_cprmask[1]` are a bit per register the object's
//! instructions name, which a linker uses to decide what a `.reginfo` of the
//! whole program holds. Both references collect them from the instructions
//! they actually encode, so an alias counts the registers its encoding has
//! and not the ones the source wrote: `nop` is `sll $zero, $zero, 0` and
//! marks `$zero`, `move $4, $5` is `or $4, $5, $zero` and marks all three,
//! and `b label` is `beq $zero, $zero, label`.
//!
//! That is also why the corpora that compare whole MIPS objects are written
//! after `.set noreorder`: llvm-mc otherwise fills a branch's delay slot
//! with a `nop`, which is an instruction more in `.text` and a `$zero` more
//! in the mask. This backend never fills a delay slot; see the module note.
//!
//! `ri_cprmask[0]`, `[2]` and `[3]` are the other coprocessors' registers,
//! and stay zero: this backend has no operand that names one. `ri_gp_value`
//! is zero in both references, which leave it to the linker.
//!
//! # Double precision on a 32-bit floating-point file
//!
//! Where the floating-point registers are 32 bits wide — a 32-bit target
//! that has not said `.module fp=64` — a double-precision value lives in an
//! even register and the odd one above it, so `add.d $f4, $f6, $f8` names
//! six registers and not three. Both references mark the whole pair for an
//! operand that holds 64 bits: the `.d` operands of the arithmetic, the
//! comparisons, `mov.d` and the conditional moves, and the register of
//! `ldc1` and `sdc1`. A single-precision or integer operand marks only
//! itself, which is why `lwc1`, `mtc1` and the `.s` forms are unchanged, and
//! why a 64-bit target or `.module fp=64` — where one register holds the
//! whole value — marks one register everywhere. A condition flag belongs to
//! no file and is counted nowhere, as `mark` says below.
//!
//! The two references part company on the operands of a *mixed*-format
//! instruction. llvm-mc reads each operand's own format, so `cvt.d.s
//! $f4, $f6` pairs only `$f4`; GNU as pairs every floating-point operand of
//! any instruction that has a double-precision form, which its own source
//! calls "overly pessimistic for things like cvt.d.s". rsasm follows
//! llvm-mc, as the README's Verification section says it does for these
//! sections.
//!
//! They part company again where the register written is odd, which both
//! warn about: GNU as keeps the number and marks the one above it, while
//! llvm-mc rounds the encoding itself down to the even half of the pair.
//! rsasm encodes the number written, as GNU as does, and marks the pair that
//! number is half of.
//!
//! # The ABI flags
//!
//! `.MIPS.abiflags` is what the default CPU needs, changed only by
//! `.module`; see `Mips::module`.

use super::reg::{Reg, RegClass};
use crate::arch::{ArchState, AttrBody, AttrSection, Endian};

/// `SHT_MIPS_REGINFO`.
const SHT_MIPS_REGINFO: u32 = 0x7000_0006;
/// `SHT_MIPS_OPTIONS`.
const SHT_MIPS_OPTIONS: u32 = 0x7000_000d;
/// `SHT_MIPS_ABIFLAGS`.
const SHT_MIPS_ABIFLAGS: u32 = 0x7000_002a;
/// `SHF_ALLOC`, and `SHF_MIPS_NOSTRIP`, which `.MIPS.options` carries so
/// that `strip` leaves it alone.
const SHF_ALLOC: u64 = 0x2;
const SHF_MIPS_NOSTRIP: u64 = 0x0800_0000;
/// `ODK_REGINFO`, the only option kind either reference writes.
const ODK_REGINFO: u8 = 1;

/// Records that an instruction's encoding names this register.
pub(crate) fn mark(state: &mut ArchState, r: Reg) {
    let bit = match r.class {
        RegClass::Gpr => u64::from(r.num),
        RegClass::Fpr => u64::from(r.num) + 32,
        // The condition flags are neither file: llvm-mc counts a register
        // towards a mask only when it belongs to one of the classes a mask
        // is about, and `$fcc0`-`$fcc7` belong to none of them.
        RegClass::Fcc => return,
    };
    state.used |= 1 << bit;
}

/// Records a floating-point operand. `wide` says it holds a 64-bit value,
/// which on a 32-bit floating-point file is the named register together with
/// the other half of its pair; see the module note.
pub(crate) fn mark_fpr(state: &mut ArchState, r: Reg, wide: bool) {
    mark(state, r);
    if wide && !fp64(state) {
        mark(state, Reg::fpr(r.num ^ 1));
    }
}

/// True where one floating-point register holds a double: on a 64-bit target
/// always, and on a 32-bit one once the source has said `.module fp=64`.
fn fp64(state: &ArchState) -> bool {
    state.bits == 64 || state.features & super::FEATURE_FP64 != 0
}

/// Records `$zero`, which the aliases that encode one put in a register
/// field the source did not write.
pub(crate) fn mark_zero(state: &mut ArchState) {
    state.used |= 1;
}

fn gpr_mask(state: &ArchState) -> u32 {
    state.used as u32
}

fn fpr_mask(state: &ArchState) -> u32 {
    (state.used >> 32) as u32
}

/// Appends a value in the target's byte order.
struct Buf {
    bytes: Vec<u8>,
    endian: Endian,
}

impl Buf {
    fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    fn u16(&mut self, v: u16) {
        match self.endian {
            Endian::Little => self.bytes.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => self.bytes.extend_from_slice(&v.to_be_bytes()),
        }
    }

    fn u32(&mut self, v: u32) {
        match self.endian {
            Endian::Little => self.bytes.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => self.bytes.extend_from_slice(&v.to_be_bytes()),
        }
    }

    fn u64(&mut self, v: u64) {
        match self.endian {
            Endian::Little => self.bytes.extend_from_slice(&v.to_le_bytes()),
            Endian::Big => self.bytes.extend_from_slice(&v.to_be_bytes()),
        }
    }
}

/// The two sections, in the order llvm-mc writes them.
pub(crate) fn sections(bits: u8, endian: Endian, state: &ArchState) -> Vec<AttrSection> {
    let mut b = Buf {
        bytes: Vec::new(),
        endian,
    };
    let reginfo = if bits == 64 {
        // n64 keeps the register information in a `.MIPS.options` entry,
        // whose header is the kind, its length, and two fields a linker
        // fills in. `Elf64_RegInfo` has a word of padding that the 32-bit
        // form has not, and a doubleword `ri_gp_value`.
        b.u8(ODK_REGINFO);
        b.u8(40);
        b.u16(0);
        b.u32(0);
        b.u32(gpr_mask(state));
        b.u32(0);
        b.u32(0);
        b.u32(fpr_mask(state));
        b.u32(0);
        b.u32(0);
        b.u64(0);
        AttrSection {
            name: ".MIPS.options",
            sh_type: SHT_MIPS_OPTIONS,
            sh_flags: SHF_ALLOC | SHF_MIPS_NOSTRIP,
            align: 8,
            entsize: 1,
            body: AttrBody::Bytes(std::mem::take(&mut b.bytes)),
        }
    } else {
        b.u32(gpr_mask(state));
        b.u32(0);
        b.u32(fpr_mask(state));
        b.u32(0);
        b.u32(0);
        b.u32(0);
        AttrSection {
            name: ".reginfo",
            sh_type: SHT_MIPS_REGINFO,
            sh_flags: SHF_ALLOC,
            align: 4,
            entsize: 24,
            body: AttrBody::Bytes(std::mem::take(&mut b.bytes)),
        }
    };

    // `Elf_Internal_ABIFlags_v0`: what a loader needs of the processor. Both
    // references write the same one for the default CPU, and `.module` is
    // what a source changes it with.
    let wide = bits == 64;
    let soft = state.features & super::FEATURE_SOFTFLOAT != 0;
    let fp64 = fp64(state);
    let odd_spreg = state.features & super::FEATURE_NO_ODD_SPREG == 0;
    // `cpr1_size` is the floating-point file: none, 32 bits or 64.
    let cpr1 = match (soft, fp64) {
        (true, _) => 0,
        (false, false) => 1,
        (false, true) => 2,
    };
    // `fp_abi` is what the file's calling convention needs of it:
    // `SOFT` (3), `64` (6) or `64A` (7) where it gave up the odd
    // single-precision registers, and `DOUBLE` (1) otherwise.
    let fp_abi = match (soft, fp64 && !wide, odd_spreg) {
        (true, _, _) => 3,
        (false, true, true) => 6,
        (false, true, false) => 7,
        _ => 1,
    };
    b.u16(0); // version
    b.u8(if wide { 64 } else { 32 }); // isa_level
    b.u8(1); // isa_rev: MIPS32r1 / MIPS64r1
    b.u8(if wide { 2 } else { 1 }); // gpr_size: AFL_REG_32 / _64
    b.u8(cpr1);
    b.u8(0); // cpr2_size: no second coprocessor
    b.u8(fp_abi);
    b.u32(0); // isa_ext
    b.u32(0); // ases
    b.u32(u32::from(odd_spreg)); // flags1: ODDSPREG
    b.u32(0); // flags2
    let abiflags = AttrSection {
        name: ".MIPS.abiflags",
        sh_type: SHT_MIPS_ABIFLAGS,
        sh_flags: SHF_ALLOC,
        align: 8,
        entsize: 24,
        body: AttrBody::Bytes(b.bytes),
    };
    vec![reginfo, abiflags]
}
