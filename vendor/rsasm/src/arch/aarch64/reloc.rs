//! ELF relocation types for AArch64 (`R_AARCH64_*`).

pub const ABS64: u32 = 257;
pub const ABS32: u32 = 258;
pub const ABS16: u32 = 259;
pub const PREL64: u32 = 260;
pub const PREL32: u32 = 261;
pub const PREL16: u32 = 262;
// One 16-bit group of an address, for `movz`/`movn`/`movk`. `UABS` counts
// the address from zero and refuses a negative one, `SABS` sign-extends it,
// and `PREL` counts it from the instruction; `_NC` drops whatever lies above
// the group rather than checking it, as the topmost group of each family
// does anyway.
pub const MOVW_UABS_G0: u32 = 263;
pub const MOVW_UABS_G0_NC: u32 = 264;
pub const MOVW_UABS_G1: u32 = 265;
pub const MOVW_UABS_G1_NC: u32 = 266;
pub const MOVW_UABS_G2: u32 = 267;
pub const MOVW_UABS_G2_NC: u32 = 268;
pub const MOVW_UABS_G3: u32 = 269;
pub const MOVW_SABS_G0: u32 = 270;
pub const MOVW_SABS_G1: u32 = 271;
pub const MOVW_SABS_G2: u32 = 272;
pub const LD_PREL_LO19: u32 = 273;
pub const ADR_PREL_LO21: u32 = 274;
pub const ADR_PREL_PG_HI21: u32 = 275;
pub const ADD_ABS_LO12_NC: u32 = 277;
pub const LDST8_ABS_LO12_NC: u32 = 278;
pub const TSTBR14: u32 = 279;
pub const CONDBR19: u32 = 280;
pub const JUMP26: u32 = 282;
pub const CALL26: u32 = 283;
pub const LDST16_ABS_LO12_NC: u32 = 284;
pub const LDST32_ABS_LO12_NC: u32 = 285;
pub const LDST64_ABS_LO12_NC: u32 = 286;
pub const MOVW_PREL_G0: u32 = 287;
pub const MOVW_PREL_G0_NC: u32 = 288;
pub const MOVW_PREL_G1: u32 = 289;
pub const MOVW_PREL_G1_NC: u32 = 290;
pub const MOVW_PREL_G2: u32 = 291;
pub const MOVW_PREL_G2_NC: u32 = 292;
pub const MOVW_PREL_G3: u32 = 293;
pub const LDST128_ABS_LO12_NC: u32 = 299;
pub const GOT_LD_PREL19: u32 = 309;
pub const ADR_GOT_PAGE: u32 = 311;
pub const LD64_GOT_LO12_NC: u32 = 312;

// The thread-local access models, one family per model: general dynamic
// (`TLSGD`), local dynamic (`TLSLD`, and the `DTPREL` offsets it adds to the
// module's block), initial exec (`TLSIE`, a GOT slot holding the offset from
// the thread pointer), local exec (`TLSLE`, that offset itself) and TLS
// descriptors. Each names the variable, never its section, and only the
// linker can compute any of them, so none is ever resolved here.
pub const TLSGD_ADR_PREL21: u32 = 512;
pub const TLSGD_ADR_PAGE21: u32 = 513;
pub const TLSGD_ADD_LO12_NC: u32 = 514;
pub const TLSGD_MOVW_G1: u32 = 515;
pub const TLSGD_MOVW_G0_NC: u32 = 516;
pub const TLSLD_ADR_PREL21: u32 = 517;
pub const TLSLD_ADR_PAGE21: u32 = 518;
pub const TLSLD_ADD_LO12_NC: u32 = 519;
pub const TLSLD_MOVW_DTPREL_G2: u32 = 523;
pub const TLSLD_MOVW_DTPREL_G1: u32 = 524;
pub const TLSLD_MOVW_DTPREL_G1_NC: u32 = 525;
pub const TLSLD_MOVW_DTPREL_G0: u32 = 526;
pub const TLSLD_MOVW_DTPREL_G0_NC: u32 = 527;
pub const TLSLD_ADD_DTPREL_HI12: u32 = 528;
pub const TLSLD_ADD_DTPREL_LO12: u32 = 529;
pub const TLSLD_ADD_DTPREL_LO12_NC: u32 = 530;
// The load/store forms come in pairs by access size, 8 to 64 bits, the
// checked one first; the field is scaled by the size, as `:lo12:`'s is.
pub const TLSLD_LDST_DTPREL_LO12: [u32; 4] = [531, 533, 535, 537];
pub const TLSLD_LDST_DTPREL_LO12_NC: [u32; 4] = [532, 534, 536, 538];
pub const TLSIE_MOVW_GOTTPREL_G1: u32 = 539;
pub const TLSIE_MOVW_GOTTPREL_G0_NC: u32 = 540;
pub const TLSIE_ADR_GOTTPREL_PAGE21: u32 = 541;
pub const TLSIE_LD64_GOTTPREL_LO12_NC: u32 = 542;
pub const TLSIE_LD_GOTTPREL_PREL19: u32 = 543;
pub const TLSLE_MOVW_TPREL_G2: u32 = 544;
pub const TLSLE_MOVW_TPREL_G1: u32 = 545;
pub const TLSLE_MOVW_TPREL_G1_NC: u32 = 546;
pub const TLSLE_MOVW_TPREL_G0: u32 = 547;
pub const TLSLE_MOVW_TPREL_G0_NC: u32 = 548;
pub const TLSLE_ADD_TPREL_HI12: u32 = 549;
pub const TLSLE_ADD_TPREL_LO12: u32 = 550;
pub const TLSLE_ADD_TPREL_LO12_NC: u32 = 551;
pub const TLSLE_LDST_TPREL_LO12: [u32; 4] = [552, 554, 556, 558];
pub const TLSLE_LDST_TPREL_LO12_NC: [u32; 4] = [553, 555, 557, 559];
pub const TLSDESC_LD_PREL19: u32 = 560;
pub const TLSDESC_ADR_PREL21: u32 = 561;
pub const TLSDESC_ADR_PAGE21: u32 = 562;
pub const TLSDESC_LD64_LO12: u32 = 563;
pub const TLSDESC_ADD_LO12: u32 = 564;
pub const TLSDESC_OFF_G1: u32 = 565;
pub const TLSDESC_OFF_G0_NC: u32 = 566;
// Marks on the instructions of a descriptor sequence, which the linker
// rewrites when it relaxes the sequence; they fill no field.
pub const TLSDESC_LDR: u32 = 567;
pub const TLSDESC_ADD: u32 = 568;
pub const TLSDESC_CALL: u32 = 569;
/// `.xword %dtprel(sym)`: the variable's offset in its module's block.
pub const TLS_DTPREL64: u32 = 1029;

/// The absolute relocation for an `n`-byte data field.
pub fn abs(n: u8) -> Option<u32> {
    Some(match n {
        2 => ABS16,
        4 => ABS32,
        8 => ABS64,
        _ => return None,
    })
}

/// The PC-relative relocation for an `n`-byte data field.
pub fn pcrel(n: u8) -> Option<u32> {
    Some(match n {
        2 => PREL16,
        4 => PREL32,
        8 => PREL64,
        _ => return None,
    })
}
