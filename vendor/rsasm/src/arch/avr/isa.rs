//! Which AVR core a file is for: the instruction-set bits, and the `-mmcu`
//! names that select a set of them.
//!
//! Both tables come from `include/opcode/avr.h` and `gas/config/tc-avr.c` of
//! binutils 2.47, transcribed rather than reconstructed. The AVR family is
//! not one instruction set but a couple of dozen overlapping ones, and which
//! instructions a device has is a property of that device, so there is
//! nowhere else to get it from.

/// A set of instruction-set feature bits, as `include/opcode/avr.h` numbers
/// them. The names below are that header's.
pub type Isa = u32;

/// In the beginning there was the AT90S1200.
pub const ISA_1200: Isa = 0x0001;
/// `lpm`.
pub const ISA_LPM: Isa = 0x0002;
/// `lpm Rd, Z[+]`.
pub const ISA_LPMX: Isa = 0x0004;
/// Has SRAM, so `ld`, `st`, `push`, `pop` and the pointer modes work.
pub const ISA_SRAM: Isa = 0x0008;
/// The reduced "AVR tiny" core: 16 registers, and the short `lds`/`sts`.
pub const ISA_TINY: Isa = 0x0010;
/// More than 8K of program memory: `jmp` and `call` exist, and `rjmp` and
/// `rcall` no longer wrap at 8K.
pub const ISA_MEGA: Isa = 0x0020;
/// The enhanced core: `mul` and its relatives.
pub const ISA_MUL: Isa = 0x0040;
/// More than 64K of program memory: `elpm`.
pub const ISA_ELPM: Isa = 0x0080;
/// `elpm Rd, Z[+]`.
pub const ISA_ELPMX: Isa = 0x0100;
/// Can program itself: `spm`.
pub const ISA_SPM: Isa = 0x0200;
/// On-chip debug: `break`.
pub const ISA_BRK: Isa = 0x0400;
/// More than 128K of program memory: `eijmp` and `eicall`.
pub const ISA_EIND: Isa = 0x0800;
/// `movw`.
pub const ISA_MOVW: Isa = 0x1000;
/// `spm Z+`.
pub const ISA_SPMX: Isa = 0x2000;
/// `des`.
pub const ISA_DES: Isa = 0x4000;
/// The read-modify-write instructions `xch`, `las`, `lac` and `lat`.
pub const ISA_RMW: Isa = 0x8000;

pub const ISA_TINY1: Isa = ISA_1200 | ISA_LPM;
pub const ISA_2XXX: Isa = ISA_TINY1 | ISA_SRAM;
pub const ISA_2XXXA: Isa = ISA_1200 | ISA_SRAM;
/// The ATtiny26, which is missing `lpm Rd, Z+`.
pub const ISA_2XXE: Isa = ISA_2XXX | ISA_LPMX;
pub const ISA_RF401: Isa = ISA_2XXX | ISA_MOVW | ISA_LPMX;
pub const ISA_TINY2: Isa = ISA_2XXX | ISA_MOVW | ISA_LPMX | ISA_SPM | ISA_BRK;
pub const ISA_M603: Isa = ISA_2XXX | ISA_MEGA;
pub const ISA_M103: Isa = ISA_M603 | ISA_ELPM;
pub const ISA_M8: Isa = ISA_2XXX | ISA_MUL | ISA_MOVW | ISA_LPMX | ISA_SPM;
pub const ISA_PWMX: Isa = ISA_M8 | ISA_BRK;
pub const ISA_M161: Isa = ISA_M603 | ISA_MUL | ISA_MOVW | ISA_LPMX | ISA_SPM;
pub const ISA_94K: Isa = ISA_M603 | ISA_MUL | ISA_MOVW | ISA_LPMX;
pub const ISA_M323: Isa = ISA_M161 | ISA_BRK;
pub const ISA_M128: Isa = ISA_M323 | ISA_ELPM | ISA_ELPMX;
pub const ISA_M256: Isa = ISA_M128 | ISA_EIND;
pub const ISA_XMEGA: Isa = ISA_M256 | ISA_SPMX | ISA_DES;
pub const ISA_XMEGAU: Isa = ISA_XMEGA | ISA_RMW;

pub const ISA_AVR1: Isa = ISA_TINY1;
pub const ISA_AVR2: Isa = ISA_2XXX;
pub const ISA_AVR25: Isa = ISA_TINY2;
pub const ISA_AVR3: Isa = ISA_M603;
pub const ISA_AVR31: Isa = ISA_M103;
pub const ISA_AVR35: Isa = ISA_AVR3 | ISA_MOVW | ISA_LPMX | ISA_SPM | ISA_BRK;
pub const ISA_AVR3_ALL: Isa = ISA_AVR3 | ISA_AVR31 | ISA_AVR35;
pub const ISA_AVR4: Isa = ISA_PWMX;
pub const ISA_AVR5: Isa = ISA_M323;
pub const ISA_AVR51: Isa = ISA_M128;
pub const ISA_AVR6: Isa = ISA_1200
    | ISA_LPM
    | ISA_LPMX
    | ISA_SRAM
    | ISA_MEGA
    | ISA_MUL
    | ISA_ELPM
    | ISA_ELPMX
    | ISA_SPM
    | ISA_BRK
    | ISA_EIND
    | ISA_MOVW;
pub const ISA_AVRTINY: Isa = ISA_1200 | ISA_BRK | ISA_SRAM | ISA_TINY;

/// The core `avr-elf-as` assembles for when no `-mmcu` is given: the plain
/// AVR2 set, with machine number 2. `.arch avr2` is not the same thing —
/// that name selects [`ISA_AVR25`] in the table below, which is what GCC 4.3
/// and later expect.
pub const DEFAULT: Mcu = Mcu {
    name: "avr",
    isa: ISA_AVR2,
    mach: 2,
};

/// A `-mmcu` name: the instruction set it has, and the machine number that
/// goes in the low seven bits of `e_flags`.
#[derive(Copy, Clone, Debug)]
pub struct Mcu {
    pub name: &'static str,
    pub isa: Isa,
    pub mach: u8,
}

/// Looks up an MCU name, case-insensitively as `md_parse_option` does.
pub fn lookup(name: &str) -> Option<Mcu> {
    MCUS.iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|&(name, isa, mach)| Mcu { name, isa, mach })
}

/// The architecture family names, which are what `--list-arch` shows and
/// what source normally writes. Every device name in [`MCUS`] is accepted as
/// well, but there are 270 of them and listing them all would say nothing.
pub const FAMILIES: &[&str] = &[
    "avr",
    "avr1",
    "avr2",
    "avr25",
    "avr3",
    "avr31",
    "avr35",
    "avr4",
    "avr5",
    "avr51",
    "avr6",
    "avrxmega1",
    "avrxmega2",
    "avrxmega3",
    "avrxmega4",
    "avrxmega5",
    "avrxmega6",
    "avrxmega7",
    "avrtiny",
];

/// `mcu_types` from `gas/config/tc-avr.c`. The comment there notes that the
/// `avr2`, `avr3` and `avr5` rows name a larger instruction set than the
/// architecture really has, for backward compatibility with GCC 4.3. Three
/// device names appear twice, with the same values; the first row wins, as
/// it does there.
#[rustfmt::skip]
pub const MCUS: &[(&str, Isa, u8)] = &[
    ("avr1", ISA_AVR1, 1),
    ("avr2", ISA_AVR25, 2),
    ("avr25", ISA_AVR25, 25),
    ("avr3", ISA_AVR3_ALL, 3),
    ("avr31", ISA_AVR31, 31),
    ("avr35", ISA_AVR35, 35),
    ("avr4", ISA_AVR4, 4),
    ("avr5", ISA_AVR51, 5),
    ("avr51", ISA_AVR51, 51),
    ("avr6", ISA_AVR6, 6),
    ("avrxmega1", ISA_XMEGA, 101),
    ("avrxmega2", ISA_XMEGA, 102),
    ("avrxmega3", ISA_XMEGA, 103),
    ("avrxmega4", ISA_XMEGA, 104),
    ("avrxmega5", ISA_XMEGA, 105),
    ("avrxmega6", ISA_XMEGA, 106),
    ("avrxmega7", ISA_XMEGA, 107),
    ("avrtiny", ISA_AVRTINY, 100),
    ("at90s1200", ISA_1200, 1),
    ("attiny11", ISA_AVR1, 1),
    ("attiny12", ISA_AVR1, 1),
    ("attiny15", ISA_AVR1, 1),
    ("attiny28", ISA_AVR1, 1),
    ("at90s2313", ISA_AVR2, 2),
    ("at90s2323", ISA_AVR2, 2),
    ("at90s2333", ISA_AVR2, 2),
    ("at90s2343", ISA_AVR2, 2),
    ("attiny22", ISA_AVR2, 2),
    ("attiny26", ISA_2XXE, 2),
    ("at90s4414", ISA_AVR2, 2),
    ("at90s4433", ISA_AVR2, 2),
    ("at90s4434", ISA_AVR2, 2),
    ("at90s8515", ISA_AVR2, 2),
    ("at90c8534", ISA_AVR2, 2),
    ("at90s8535", ISA_AVR2, 2),
    ("ata5272", ISA_AVR25, 25),
    ("attiny13", ISA_AVR25, 25),
    ("attiny13a", ISA_AVR25, 25),
    ("attiny2313", ISA_AVR25, 25),
    ("attiny2313a", ISA_AVR25, 25),
    ("attiny24", ISA_AVR25, 25),
    ("attiny24a", ISA_AVR25, 25),
    ("attiny4313", ISA_AVR25, 25),
    ("attiny44", ISA_AVR25, 25),
    ("attiny44a", ISA_AVR25, 25),
    ("attiny84", ISA_AVR25, 25),
    ("attiny84a", ISA_AVR25, 25),
    ("attiny25", ISA_AVR25, 25),
    ("attiny45", ISA_AVR25, 25),
    ("attiny85", ISA_AVR25, 25),
    ("attiny261", ISA_AVR25, 25),
    ("attiny261a", ISA_AVR25, 25),
    ("attiny461", ISA_AVR25, 25),
    ("attiny461a", ISA_AVR25, 25),
    ("attiny861", ISA_AVR25, 25),
    ("attiny861a", ISA_AVR25, 25),
    ("attiny87", ISA_AVR25, 25),
    ("attiny43u", ISA_AVR25, 25),
    ("attiny48", ISA_AVR25, 25),
    ("attiny88", ISA_AVR25, 25),
    ("attiny828", ISA_AVR25, 25),
    ("at86rf401", ISA_RF401, 25),
    ("at43usb355", ISA_AVR3, 3),
    ("at76c711", ISA_AVR3, 3),
    ("atmega103", ISA_AVR31, 31),
    ("at43usb320", ISA_AVR31, 31),
    ("attiny167", ISA_AVR35, 35),
    ("at90usb82", ISA_AVR35, 35),
    ("at90usb162", ISA_AVR35, 35),
    ("ata5505", ISA_AVR35, 35),
    ("atmega8u2", ISA_AVR35, 35),
    ("atmega16u2", ISA_AVR35, 35),
    ("atmega32u2", ISA_AVR35, 35),
    ("attiny1634", ISA_AVR35, 35),
    ("atmega8", ISA_M8, 4),
    ("ata6289", ISA_AVR4, 4),
    ("atmega8a", ISA_M8, 4),
    ("ata6285", ISA_AVR4, 4),
    ("ata6286", ISA_AVR4, 4),
    ("atmega48", ISA_AVR4, 4),
    ("atmega48a", ISA_AVR4, 4),
    ("atmega48pa", ISA_AVR4, 4),
    ("atmega48p", ISA_AVR4, 4),
    ("atmega88", ISA_AVR4, 4),
    ("atmega88a", ISA_AVR4, 4),
    ("atmega88p", ISA_AVR4, 4),
    ("atmega88pa", ISA_AVR4, 4),
    ("atmega8515", ISA_M8, 4),
    ("atmega8535", ISA_M8, 4),
    ("atmega8hva", ISA_AVR4, 4),
    ("at90pwm1", ISA_AVR4, 4),
    ("at90pwm2", ISA_AVR4, 4),
    ("at90pwm2b", ISA_AVR4, 4),
    ("at90pwm3", ISA_AVR4, 4),
    ("at90pwm3b", ISA_AVR4, 4),
    ("at90pwm81", ISA_AVR4, 4),
    ("at90pwm161", ISA_AVR5, 5),
    ("ata5790", ISA_AVR5, 5),
    ("ata5795", ISA_AVR5, 5),
    ("atmega16", ISA_AVR5, 5),
    ("atmega16a", ISA_AVR5, 5),
    ("atmega161", ISA_M161, 5),
    ("atmega162", ISA_AVR5, 5),
    ("atmega163", ISA_M161, 5),
    ("atmega164a", ISA_AVR5, 5),
    ("atmega164p", ISA_AVR5, 5),
    ("atmega164pa", ISA_AVR5, 5),
    ("atmega165", ISA_AVR5, 5),
    ("atmega165a", ISA_AVR5, 5),
    ("atmega165p", ISA_AVR5, 5),
    ("atmega165pa", ISA_AVR5, 5),
    ("atmega168", ISA_AVR5, 5),
    ("atmega168a", ISA_AVR5, 5),
    ("atmega168p", ISA_AVR5, 5),
    ("atmega168pa", ISA_AVR5, 5),
    ("atmega169", ISA_AVR5, 5),
    ("atmega169a", ISA_AVR5, 5),
    ("atmega169p", ISA_AVR5, 5),
    ("atmega169pa", ISA_AVR5, 5),
    ("atmega32", ISA_AVR5, 5),
    ("atmega32a", ISA_AVR5, 5),
    ("atmega323", ISA_AVR5, 5),
    ("atmega324a", ISA_AVR5, 5),
    ("atmega324p", ISA_AVR5, 5),
    ("atmega324pa", ISA_AVR5, 5),
    ("atmega325", ISA_AVR5, 5),
    ("atmega325a", ISA_AVR5, 5),
    ("atmega325p", ISA_AVR5, 5),
    ("atmega325pa", ISA_AVR5, 5),
    ("atmega3250", ISA_AVR5, 5),
    ("atmega3250a", ISA_AVR5, 5),
    ("atmega3250p", ISA_AVR5, 5),
    ("atmega3250pa", ISA_AVR5, 5),
    ("atmega328", ISA_AVR5, 5),
    ("atmega328p", ISA_AVR5, 5),
    ("atmega329", ISA_AVR5, 5),
    ("atmega329a", ISA_AVR5, 5),
    ("atmega329p", ISA_AVR5, 5),
    ("atmega329pa", ISA_AVR5, 5),
    ("atmega3290", ISA_AVR5, 5),
    ("atmega3290a", ISA_AVR5, 5),
    ("atmega3290p", ISA_AVR5, 5),
    ("atmega3290pa", ISA_AVR5, 5),
    ("atmega406", ISA_AVR5, 5),
    ("atmega64rfr2", ISA_AVR5, 5),
    ("atmega644rfr2", ISA_AVR5, 5),
    ("atmega64", ISA_AVR5, 5),
    ("atmega64a", ISA_AVR5, 5),
    ("atmega640", ISA_AVR5, 5),
    ("atmega644", ISA_AVR5, 5),
    ("atmega644a", ISA_AVR5, 5),
    ("atmega644p", ISA_AVR5, 5),
    ("atmega644pa", ISA_AVR5, 5),
    ("atmega645", ISA_AVR5, 5),
    ("atmega645a", ISA_AVR5, 5),
    ("atmega645p", ISA_AVR5, 5),
    ("atmega649", ISA_AVR5, 5),
    ("atmega649a", ISA_AVR5, 5),
    ("atmega649p", ISA_AVR5, 5),
    ("atmega6450", ISA_AVR5, 5),
    ("atmega6450a", ISA_AVR5, 5),
    ("atmega6450p", ISA_AVR5, 5),
    ("atmega6490", ISA_AVR5, 5),
    ("atmega6490a", ISA_AVR5, 5),
    ("atmega6490p", ISA_AVR5, 5),
    ("atmega64rfr2", ISA_AVR5, 5),
    ("atmega644rfr2", ISA_AVR5, 5),
    ("atmega16hva", ISA_AVR5, 5),
    ("atmega16hva2", ISA_AVR5, 5),
    ("atmega16hvb", ISA_AVR5, 5),
    ("atmega16hvbrevb", ISA_AVR5, 5),
    ("atmega32hvb", ISA_AVR5, 5),
    ("atmega32hvbrevb", ISA_AVR5, 5),
    ("atmega64hve", ISA_AVR5, 5),
    ("at90can32", ISA_AVR5, 5),
    ("at90can64", ISA_AVR5, 5),
    ("at90pwm161", ISA_AVR5, 5),
    ("at90pwm216", ISA_AVR5, 5),
    ("at90pwm316", ISA_AVR5, 5),
    ("atmega32c1", ISA_AVR5, 5),
    ("atmega64c1", ISA_AVR5, 5),
    ("atmega16m1", ISA_AVR5, 5),
    ("atmega32m1", ISA_AVR5, 5),
    ("atmega64m1", ISA_AVR5, 5),
    ("atmega16u4", ISA_AVR5, 5),
    ("atmega32u4", ISA_AVR5, 5),
    ("atmega32u6", ISA_AVR5, 5),
    ("at90usb646", ISA_AVR5, 5),
    ("at90usb647", ISA_AVR5, 5),
    ("at90scr100", ISA_AVR5, 5),
    ("at94k", ISA_94K, 5),
    ("m3000", ISA_AVR5, 5),
    ("atmega128", ISA_AVR51, 51),
    ("atmega128a", ISA_AVR51, 51),
    ("atmega1280", ISA_AVR51, 51),
    ("atmega1281", ISA_AVR51, 51),
    ("atmega1284", ISA_AVR51, 51),
    ("atmega1284p", ISA_AVR51, 51),
    ("atmega128rfa1", ISA_AVR51, 51),
    ("atmega128rfr2", ISA_AVR51, 51),
    ("atmega1284rfr2", ISA_AVR51, 51),
    ("at90can128", ISA_AVR51, 51),
    ("at90usb1286", ISA_AVR51, 51),
    ("at90usb1287", ISA_AVR51, 51),
    ("atmega2560", ISA_AVR6, 6),
    ("atmega2561", ISA_AVR6, 6),
    ("atmega256rfr2", ISA_AVR6, 6),
    ("atmega2564rfr2", ISA_AVR6, 6),
    ("atxmega16a4", ISA_XMEGA, 102),
    ("atxmega16a4u", ISA_XMEGAU, 102),
    ("atxmega16c4", ISA_XMEGAU, 102),
    ("atxmega16d4", ISA_XMEGA, 102),
    ("atxmega32a4", ISA_XMEGA, 102),
    ("atxmega32a4u", ISA_XMEGAU, 102),
    ("atxmega32c4", ISA_XMEGAU, 102),
    ("atxmega32d4", ISA_XMEGA, 102),
    ("atxmega32e5", ISA_XMEGA, 102),
    ("atxmega16e5", ISA_XMEGA, 102),
    ("atxmega8e5", ISA_XMEGA, 102),
    ("atxmega32x1", ISA_XMEGA, 102),
    ("attiny212", ISA_XMEGA, 103),
    ("attiny214", ISA_XMEGA, 103),
    ("attiny412", ISA_XMEGA, 103),
    ("attiny414", ISA_XMEGA, 103),
    ("attiny416", ISA_XMEGA, 103),
    ("attiny417", ISA_XMEGA, 103),
    ("attiny814", ISA_XMEGA, 103),
    ("attiny816", ISA_XMEGA, 103),
    ("attiny817", ISA_XMEGA, 103),
    ("attiny1614", ISA_XMEGA, 103),
    ("attiny1616", ISA_XMEGA, 103),
    ("attiny1617", ISA_XMEGA, 103),
    ("attiny3214", ISA_XMEGA, 103),
    ("attiny3216", ISA_XMEGA, 103),
    ("attiny3217", ISA_XMEGA, 103),
    ("atxmega64a3", ISA_XMEGA, 104),
    ("atxmega64a3u", ISA_XMEGAU, 104),
    ("atxmega64a4u", ISA_XMEGAU, 104),
    ("atxmega64b1", ISA_XMEGAU, 104),
    ("atxmega64b3", ISA_XMEGAU, 104),
    ("atxmega64c3", ISA_XMEGAU, 104),
    ("atxmega64d3", ISA_XMEGA, 104),
    ("atxmega64d4", ISA_XMEGA, 104),
    ("atxmega64a1", ISA_XMEGA, 105),
    ("atxmega64a1u", ISA_XMEGAU, 105),
    ("atxmega128a3", ISA_XMEGA, 106),
    ("atxmega128a3u", ISA_XMEGAU, 106),
    ("atxmega128b1", ISA_XMEGAU, 106),
    ("atxmega128b3", ISA_XMEGAU, 106),
    ("atxmega128c3", ISA_XMEGAU, 106),
    ("atxmega128d3", ISA_XMEGA, 106),
    ("atxmega128d4", ISA_XMEGA, 106),
    ("atxmega192a3", ISA_XMEGA, 106),
    ("atxmega192a3u", ISA_XMEGAU, 106),
    ("atxmega192c3", ISA_XMEGAU, 106),
    ("atxmega192d3", ISA_XMEGA, 106),
    ("atxmega256a3", ISA_XMEGA, 106),
    ("atxmega256a3u", ISA_XMEGAU, 106),
    ("atxmega256a3b", ISA_XMEGA, 106),
    ("atxmega256a3bu", ISA_XMEGAU, 106),
    ("atxmega256c3", ISA_XMEGAU, 106),
    ("atxmega256d3", ISA_XMEGA, 106),
    ("atxmega384c3", ISA_XMEGAU, 106),
    ("atxmega384d3", ISA_XMEGA, 106),
    ("atxmega128a1", ISA_XMEGA, 107),
    ("atxmega128a1u", ISA_XMEGAU, 107),
    ("atxmega128a4u", ISA_XMEGAU, 107),
    ("attiny4", ISA_AVRTINY, 100),
    ("attiny5", ISA_AVRTINY, 100),
    ("attiny9", ISA_AVRTINY, 100),
    ("attiny10", ISA_AVRTINY, 100),
    ("attiny20", ISA_AVRTINY, 100),
    ("attiny40", ISA_AVRTINY, 100),
];
