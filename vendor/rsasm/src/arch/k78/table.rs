//! The 78K0 instruction code table, transcribed row for row from NEC's manual.
//!
//! # Source
//!
//! NEC Corporation, *78K/0 Series User's Manual: Instructions*, document
//! U12326EJ4V0UM00 (4th edition, October 2001), **section 4.2.2 "Instruction
//! code list", pages 39 to 45**. The legend for the bit names (`R2 R1 R0`,
//! `P1 P0`, `B2 B1 B0`, `RB1 RB0`, `fa10–8`, `ta4–0`) and for the named byte
//! fields is section 4.2.1, page 38. The copy used was the one NEC distributed
//! through Farnell (`farnell.com/datasheets/34134.pdf`, SHA-256
//! `26f39c7a9cd4c9e206791ecf781661cb9d38da4f1fb095d2c58edfc1ada8f14f`).
//!
//! Every [`Row`] is one line of that table: the page it is printed on, the
//! mnemonic, the operand column exactly as printed (without the spaces), the
//! footnote marker, and one string per `B1`..`B4` column. To check a row, open
//! the page and read across.
//!
//! The rows were extracted mechanically from the PDF's text layer and then
//! compared against the rendered pages, so they carry the manual's own
//! grouping and order — including its quirks, such as `SET1 CY` being listed
//! after `CLR1 [HL].bit`, and the `MOVW SP` forms sitting under "Stack
//! Manipulation" rather than with the other `MOVW` rows.
//!
//! # Notation
//!
//! A code column is either a named field, spelled as the manual spells it
//! (`Data`, `Low byte`, `High byte`, `Saddr-offset`, `Sfr-offset`, `Low addr`,
//! `High addr`, `jdisp`, `fa7-0`), or eight bit positions, most significant
//! first, split into two nibbles for legibility. A bit position is `0` or `1`,
//! or a letter standing for one of the manual's bit names:
//!
//! | letter | manual        | operand     |
//! |--------|---------------|-------------|
//! | `r`    | `R2 R1 R0`    | `r`         |
//! | `p`    | `P1 P0`       | `rp`        |
//! | `b`    | `B2 B1 B0`    | `.bit`      |
//! | `n`    | `RB1`, `RB0`  | `RBn`       |
//! | `f`    | `fa10–8`      | `!addr11`   |
//! | `t`    | `ta4–0`       | `[addr5]`   |
//!
//! Repeated letters take the value's bits most significant first, wherever
//! they sit in the byte: `SEL RBn` is printed `1 1 RB1 1 RB0 0 0 0`, which is
//! `"11n1 n000"` here.
//!
//! The two footnotes are the manual's own: *Except r = A* (on the 8-bit
//! register-to-accumulator forms, whose `r = A` codes are the `31H`, `61H` and
//! `71H` prefix bytes) and *Only when rp = BC, DE or HL* (on `MOVW AX,rp`,
//! `MOVW rp,AX` and `XCHW AX,rp`).
//!
//! # Byte counts
//!
//! The instruction lengths implied by these rows are checked, in
//! `tests/k78.rs`, against a second NEC/Renesas document: the "Bytes" column of
//! the operation list in the *78K0/Kx2 User's Manual: Hardware*,
//! R01UH0008EJ0401 (Rev. 4.01, July 2010), section 29.2, pages 761 to 768.

/// A footnote attached to a row of the code table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Note {
    None,
    /// "Except r = A."
    ExceptA,
    /// "Only when rp = BC, DE or HL."
    OnlyBcDeHl,
}

const NONE: Note = Note::None;
const EXCEPT_A: Note = Note::ExceptA;
const BC_DE_HL: Note = Note::OnlyBcDeHl;

/// One line of U12326EJ4V0UM section 4.2.2.
#[derive(Copy, Clone, Debug)]
pub struct Row {
    /// The page of U12326EJ4V0UM the row is printed on.
    pub page: u16,
    pub mnemonic: &'static str,
    /// The "Operands" column, spaces removed; empty for none.
    pub operands: &'static str,
    pub note: Note,
    /// The `B1`..`B4` columns.
    pub codes: &'static [&'static str],
}

const fn row(
    page: u16,
    mnemonic: &'static str,
    operands: &'static str,
    note: Note,
    codes: &'static [&'static str],
) -> Row {
    Row {
        page,
        mnemonic,
        operands,
        note,
        codes,
    }
}

/// The whole table, in the manual's order.
#[rustfmt::skip]
pub const ROWS: &[Row] = &[
    // Page 39: 8-bit data transfer.
    row(39, "MOV", "r,#byte", NONE, &["1010 0rrr", "Data"]),
    row(39, "MOV", "saddr,#byte", NONE, &["0001 0001", "Saddr-offset", "Data"]),
    row(39, "MOV", "sfr,#byte", NONE, &["0001 0011", "Sfr-offset", "Data"]),
    row(39, "MOV", "A,r", EXCEPT_A, &["0110 0rrr"]),
    row(39, "MOV", "r,A", EXCEPT_A, &["0111 0rrr"]),
    row(39, "MOV", "A,saddr", NONE, &["1111 0000", "Saddr-offset"]),
    row(39, "MOV", "saddr,A", NONE, &["1111 0010", "Saddr-offset"]),
    row(39, "MOV", "A,sfr", NONE, &["1111 0100", "Sfr-offset"]),
    row(39, "MOV", "sfr,A", NONE, &["1111 0110", "Sfr-offset"]),
    row(39, "MOV", "A,!addr16", NONE, &["1000 1110", "Low addr", "High addr"]),
    row(39, "MOV", "!addr16,A", NONE, &["1001 1110", "Low addr", "High addr"]),
    row(39, "MOV", "PSW,#byte", NONE, &["0001 0001", "0001 1110", "Data"]),
    row(39, "MOV", "A,PSW", NONE, &["1111 0000", "0001 1110"]),
    row(39, "MOV", "PSW,A", NONE, &["1111 0010", "0001 1110"]),
    row(39, "MOV", "A,[DE]", NONE, &["1000 0101"]),
    row(39, "MOV", "[DE],A", NONE, &["1001 0101"]),
    row(39, "MOV", "A,[HL]", NONE, &["1000 0111"]),
    row(39, "MOV", "[HL],A", NONE, &["1001 0111"]),
    row(39, "MOV", "A,[HL+byte]", NONE, &["1010 1110", "Data"]),
    row(39, "MOV", "[HL+byte],A", NONE, &["1011 1110", "Data"]),
    row(39, "MOV", "A,[HL+B]", NONE, &["1010 1011"]),
    row(39, "MOV", "[HL+B],A", NONE, &["1011 1011"]),
    row(39, "MOV", "A,[HL+C]", NONE, &["1010 1010"]),
    row(39, "MOV", "[HL+C],A", NONE, &["1011 1010"]),
    row(39, "XCH", "A,r", EXCEPT_A, &["0011 0rrr"]),
    row(39, "XCH", "A,saddr", NONE, &["1000 0011", "Saddr-offset"]),
    row(39, "XCH", "A,sfr", NONE, &["1001 0011", "Sfr-offset"]),
    row(39, "XCH", "A,!addr16", NONE, &["1100 1110", "Low addr", "High addr"]),
    row(39, "XCH", "A,[DE]", NONE, &["0000 0101"]),
    row(39, "XCH", "A,[HL]", NONE, &["0000 0111"]),
    row(39, "XCH", "A,[HL+byte]", NONE, &["1101 1110", "Data"]),
    row(39, "XCH", "A,[HL+B]", NONE, &["0011 0001", "1000 1011"]),
    row(39, "XCH", "A,[HL+C]", NONE, &["0011 0001", "1000 1010"]),

    // Page 40: 16-bit data transfer, 8-bit operation.
    row(40, "MOVW", "rp,#word", NONE, &["0001 0pp0", "Low byte", "High byte"]),
    row(40, "MOVW", "saddrp,#word", NONE, &["1110 1110", "Saddr-offset", "Low byte", "High byte"]),
    row(40, "MOVW", "sfrp,#word", NONE, &["1111 1110", "Sfr-offset", "Low byte", "High byte"]),
    row(40, "MOVW", "AX,saddrp", NONE, &["1000 1001", "Saddr-offset"]),
    row(40, "MOVW", "saddrp,AX", NONE, &["1001 1001", "Saddr-offset"]),
    row(40, "MOVW", "AX,sfrp", NONE, &["1010 1001", "Sfr-offset"]),
    row(40, "MOVW", "sfrp,AX", NONE, &["1011 1001", "Sfr-offset"]),
    row(40, "MOVW", "AX,rp", BC_DE_HL, &["1100 0pp0"]),
    row(40, "MOVW", "rp,AX", BC_DE_HL, &["1101 0pp0"]),
    row(40, "MOVW", "AX,!addr16", NONE, &["0000 0010", "Low addr", "High addr"]),
    row(40, "MOVW", "!addr16,AX", NONE, &["0000 0011", "Low addr", "High addr"]),
    row(40, "XCHW", "AX,rp", BC_DE_HL, &["1110 0pp0"]),
    row(40, "ADD", "A,#byte", NONE, &["0000 1101", "Data"]),
    row(40, "ADD", "saddr,#byte", NONE, &["1000 1000", "Saddr-offset", "Data"]),
    row(40, "ADD", "A,r", EXCEPT_A, &["0110 0001", "0000 1rrr"]),
    row(40, "ADD", "r,A", NONE, &["0110 0001", "0000 0rrr"]),
    row(40, "ADD", "A,saddr", NONE, &["0000 1110", "Saddr-offset"]),
    row(40, "ADD", "A,!addr16", NONE, &["0000 1000", "Low addr", "High addr"]),
    row(40, "ADD", "A,[HL]", NONE, &["0000 1111"]),
    row(40, "ADD", "A,[HL+byte]", NONE, &["0000 1001", "Data"]),
    row(40, "ADD", "A,[HL+B]", NONE, &["0011 0001", "0000 1011"]),
    row(40, "ADD", "A,[HL+C]", NONE, &["0011 0001", "0000 1010"]),
    row(40, "ADDC", "A,#byte", NONE, &["0010 1101", "Data"]),
    row(40, "ADDC", "saddr,#byte", NONE, &["1010 1000", "Saddr-offset", "Data"]),
    row(40, "ADDC", "A,r", EXCEPT_A, &["0110 0001", "0010 1rrr"]),
    row(40, "ADDC", "r,A", NONE, &["0110 0001", "0010 0rrr"]),
    row(40, "ADDC", "A,saddr", NONE, &["0010 1110", "Saddr-offset"]),
    row(40, "ADDC", "A,!addr16", NONE, &["0010 1000", "Low addr", "High addr"]),
    row(40, "ADDC", "A,[HL]", NONE, &["0010 1111"]),
    row(40, "ADDC", "A,[HL+byte]", NONE, &["0010 1001", "Data"]),
    row(40, "ADDC", "A,[HL+B]", NONE, &["0011 0001", "0010 1011"]),
    row(40, "ADDC", "A,[HL+C]", NONE, &["0011 0001", "0010 1010"]),

    // Page 41: 8-bit operation.
    row(41, "SUB", "A,#byte", NONE, &["0001 1101", "Data"]),
    row(41, "SUB", "saddr,#byte", NONE, &["1001 1000", "Saddr-offset", "Data"]),
    row(41, "SUB", "A,r", EXCEPT_A, &["0110 0001", "0001 1rrr"]),
    row(41, "SUB", "r,A", NONE, &["0110 0001", "0001 0rrr"]),
    row(41, "SUB", "A,saddr", NONE, &["0001 1110", "Saddr-offset"]),
    row(41, "SUB", "A,!addr16", NONE, &["0001 1000", "Low addr", "High addr"]),
    row(41, "SUB", "A,[HL]", NONE, &["0001 1111"]),
    row(41, "SUB", "A,[HL+byte]", NONE, &["0001 1001", "Data"]),
    row(41, "SUB", "A,[HL+B]", NONE, &["0011 0001", "0001 1011"]),
    row(41, "SUB", "A,[HL+C]", NONE, &["0011 0001", "0001 1010"]),
    row(41, "SUBC", "A,#byte", NONE, &["0011 1101", "Data"]),
    row(41, "SUBC", "saddr,#byte", NONE, &["1011 1000", "Saddr-offset", "Data"]),
    row(41, "SUBC", "A,r", EXCEPT_A, &["0110 0001", "0011 1rrr"]),
    row(41, "SUBC", "r,A", NONE, &["0110 0001", "0011 0rrr"]),
    row(41, "SUBC", "A,saddr", NONE, &["0011 1110", "Saddr-offset"]),
    row(41, "SUBC", "A,!addr16", NONE, &["0011 1000", "Low addr", "High addr"]),
    row(41, "SUBC", "A,[HL]", NONE, &["0011 1111"]),
    row(41, "SUBC", "A,[HL+byte]", NONE, &["0011 1001", "Data"]),
    row(41, "SUBC", "A,[HL+B]", NONE, &["0011 0001", "0011 1011"]),
    row(41, "SUBC", "A,[HL+C]", NONE, &["0011 0001", "0011 1010"]),
    row(41, "AND", "A,#byte", NONE, &["0101 1101", "Data"]),
    row(41, "AND", "saddr,#byte", NONE, &["1101 1000", "Saddr-offset", "Data"]),
    row(41, "AND", "A,r", EXCEPT_A, &["0110 0001", "0101 1rrr"]),
    row(41, "AND", "r,A", NONE, &["0110 0001", "0101 0rrr"]),
    row(41, "AND", "A,saddr", NONE, &["0101 1110", "Saddr-offset"]),
    row(41, "AND", "A,!addr16", NONE, &["0101 1000", "Low addr", "High addr"]),
    row(41, "AND", "A,[HL]", NONE, &["0101 1111"]),
    row(41, "AND", "A,[HL+byte]", NONE, &["0101 1001", "Data"]),
    row(41, "AND", "A,[HL+B]", NONE, &["0011 0001", "0101 1011"]),
    row(41, "AND", "A,[HL+C]", NONE, &["0011 0001", "0101 1010"]),

    // Page 42: 8-bit operation.
    row(42, "OR", "A,#byte", NONE, &["0110 1101", "Data"]),
    row(42, "OR", "saddr,#byte", NONE, &["1110 1000", "Saddr-offset", "Data"]),
    row(42, "OR", "A,r", EXCEPT_A, &["0110 0001", "0110 1rrr"]),
    row(42, "OR", "r,A", NONE, &["0110 0001", "0110 0rrr"]),
    row(42, "OR", "A,saddr", NONE, &["0110 1110", "Saddr-offset"]),
    row(42, "OR", "A,!addr16", NONE, &["0110 1000", "Low addr", "High addr"]),
    row(42, "OR", "A,[HL]", NONE, &["0110 1111"]),
    row(42, "OR", "A,[HL+byte]", NONE, &["0110 1001", "Data"]),
    row(42, "OR", "A,[HL+B]", NONE, &["0011 0001", "0110 1011"]),
    row(42, "OR", "A,[HL+C]", NONE, &["0011 0001", "0110 1010"]),
    row(42, "XOR", "A,#byte", NONE, &["0111 1101", "Data"]),
    row(42, "XOR", "saddr,#byte", NONE, &["1111 1000", "Saddr-offset", "Data"]),
    row(42, "XOR", "A,r", EXCEPT_A, &["0110 0001", "0111 1rrr"]),
    row(42, "XOR", "r,A", NONE, &["0110 0001", "0111 0rrr"]),
    row(42, "XOR", "A,saddr", NONE, &["0111 1110", "Saddr-offset"]),
    row(42, "XOR", "A,!addr16", NONE, &["0111 1000", "Low addr", "High addr"]),
    row(42, "XOR", "A,[HL]", NONE, &["0111 1111"]),
    row(42, "XOR", "A,[HL+byte]", NONE, &["0111 1001", "Data"]),
    row(42, "XOR", "A,[HL+B]", NONE, &["0011 0001", "0111 1011"]),
    row(42, "XOR", "A,[HL+C]", NONE, &["0011 0001", "0111 1010"]),
    row(42, "CMP", "A,#byte", NONE, &["0100 1101", "Data"]),
    row(42, "CMP", "saddr,#byte", NONE, &["1100 1000", "Saddr-offset", "Data"]),
    row(42, "CMP", "A,r", EXCEPT_A, &["0110 0001", "0100 1rrr"]),
    row(42, "CMP", "r,A", NONE, &["0110 0001", "0100 0rrr"]),
    row(42, "CMP", "A,saddr", NONE, &["0100 1110", "Saddr-offset"]),
    row(42, "CMP", "A,!addr16", NONE, &["0100 1000", "Low addr", "High addr"]),
    row(42, "CMP", "A,[HL]", NONE, &["0100 1111"]),
    row(42, "CMP", "A,[HL+byte]", NONE, &["0100 1001", "Data"]),
    row(42, "CMP", "A,[HL+B]", NONE, &["0011 0001", "0100 1011"]),
    row(42, "CMP", "A,[HL+C]", NONE, &["0011 0001", "0100 1010"]),

    // Page 43: 16-bit operation, multiply/divide, increment/decrement,
    // rotate, BCD adjust, bit manipulation.
    row(43, "ADDW", "AX,#word", NONE, &["1100 1010", "Low byte", "High byte"]),
    row(43, "SUBW", "AX,#word", NONE, &["1101 1010", "Low byte", "High byte"]),
    row(43, "CMPW", "AX,#word", NONE, &["1110 1010", "Low byte", "High byte"]),
    row(43, "MULU", "X", NONE, &["0011 0001", "1000 1000"]),
    row(43, "DIVUW", "C", NONE, &["0011 0001", "1000 0010"]),
    row(43, "INC", "r", NONE, &["0100 0rrr"]),
    row(43, "INC", "saddr", NONE, &["1000 0001", "Saddr-offset"]),
    row(43, "DEC", "r", NONE, &["0101 0rrr"]),
    row(43, "DEC", "saddr", NONE, &["1001 0001", "Saddr-offset"]),
    row(43, "INCW", "rp", NONE, &["1000 0pp0"]),
    row(43, "DECW", "rp", NONE, &["1001 0pp0"]),
    row(43, "ROR", "A,1", NONE, &["0010 0100"]),
    row(43, "ROL", "A,1", NONE, &["0010 0110"]),
    row(43, "RORC", "A,1", NONE, &["0010 0101"]),
    row(43, "ROLC", "A,1", NONE, &["0010 0111"]),
    row(43, "ROR4", "[HL]", NONE, &["0011 0001", "1001 0000"]),
    row(43, "ROL4", "[HL]", NONE, &["0011 0001", "1000 0000"]),
    row(43, "ADJBA", "", NONE, &["0110 0001", "1000 0000"]),
    row(43, "ADJBS", "", NONE, &["0110 0001", "1001 0000"]),
    row(43, "MOV1", "CY,saddr.bit", NONE, &["0111 0001", "0bbb 0100", "Saddr-offset"]),
    row(43, "MOV1", "CY,sfr.bit", NONE, &["0111 0001", "0bbb 1100", "Sfr-offset"]),
    row(43, "MOV1", "CY,A.bit", NONE, &["0110 0001", "1bbb 1100"]),
    row(43, "MOV1", "CY,PSW.bit", NONE, &["0111 0001", "0bbb 0100", "0001 1110"]),
    row(43, "MOV1", "CY,[HL].bit", NONE, &["0111 0001", "1bbb 0100"]),
    row(43, "MOV1", "saddr.bit,CY", NONE, &["0111 0001", "0bbb 0001", "Saddr-offset"]),
    row(43, "MOV1", "sfr.bit,CY", NONE, &["0111 0001", "0bbb 1001", "Sfr-offset"]),
    row(43, "MOV1", "A.bit,CY", NONE, &["0110 0001", "1bbb 1001"]),
    row(43, "MOV1", "PSW.bit,CY", NONE, &["0111 0001", "0bbb 0001", "0001 1110"]),
    row(43, "MOV1", "[HL].bit,CY", NONE, &["0111 0001", "1bbb 0001"]),
    row(43, "AND1", "CY,saddr.bit", NONE, &["0111 0001", "0bbb 0101", "Saddr-offset"]),
    row(43, "AND1", "CY,sfr.bit", NONE, &["0111 0001", "0bbb 1101", "Sfr-offset"]),
    row(43, "AND1", "CY,A.bit", NONE, &["0110 0001", "1bbb 1101"]),
    row(43, "AND1", "CY,PSW.bit", NONE, &["0111 0001", "0bbb 0101", "0001 1110"]),
    row(43, "AND1", "CY,[HL].bit", NONE, &["0111 0001", "1bbb 0101"]),

    // Page 44: bit manipulation, call/return, stack manipulation.
    row(44, "OR1", "CY,saddr.bit", NONE, &["0111 0001", "0bbb 0110", "Saddr-offset"]),
    row(44, "OR1", "CY,sfr.bit", NONE, &["0111 0001", "0bbb 1110", "Sfr-offset"]),
    row(44, "OR1", "CY,A.bit", NONE, &["0110 0001", "1bbb 1110"]),
    row(44, "OR1", "CY,PSW.bit", NONE, &["0111 0001", "0bbb 0110", "0001 1110"]),
    row(44, "OR1", "CY,[HL].bit", NONE, &["0111 0001", "1bbb 0110"]),
    row(44, "XOR1", "CY,saddr.bit", NONE, &["0111 0001", "0bbb 0111", "Saddr-offset"]),
    row(44, "XOR1", "CY,sfr.bit", NONE, &["0111 0001", "0bbb 1111", "Sfr-offset"]),
    row(44, "XOR1", "CY,A.bit", NONE, &["0110 0001", "1bbb 1111"]),
    row(44, "XOR1", "CY,PSW.bit", NONE, &["0111 0001", "0bbb 0111", "0001 1110"]),
    row(44, "XOR1", "CY,[HL].bit", NONE, &["0111 0001", "1bbb 0111"]),
    row(44, "SET1", "saddr.bit", NONE, &["0bbb 1010", "Saddr-offset"]),
    row(44, "SET1", "sfr.bit", NONE, &["0111 0001", "0bbb 1010", "Sfr-offset"]),
    row(44, "SET1", "A.bit", NONE, &["0110 0001", "1bbb 1010"]),
    row(44, "SET1", "PSW.bit", NONE, &["0bbb 1010", "0001 1110"]),
    row(44, "SET1", "[HL].bit", NONE, &["0111 0001", "1bbb 0010"]),
    row(44, "CLR1", "saddr.bit", NONE, &["0bbb 1011", "Saddr-offset"]),
    row(44, "CLR1", "sfr.bit", NONE, &["0111 0001", "0bbb 1011", "Sfr-offset"]),
    row(44, "CLR1", "A.bit", NONE, &["0110 0001", "1bbb 1011"]),
    row(44, "CLR1", "PSW.bit", NONE, &["0bbb 1011", "0001 1110"]),
    row(44, "CLR1", "[HL].bit", NONE, &["0111 0001", "1bbb 0011"]),
    row(44, "SET1", "CY", NONE, &["0010 0000"]),
    row(44, "CLR1", "CY", NONE, &["0010 0001"]),
    row(44, "NOT1", "CY", NONE, &["0000 0001"]),
    row(44, "CALL", "!addr16", NONE, &["1001 1010", "Low addr", "High addr"]),
    row(44, "CALLF", "!addr11", NONE, &["0fff 1100", "fa7-0"]),
    row(44, "CALLT", "[addr5]", NONE, &["11tt ttt1"]),
    row(44, "BRK", "", NONE, &["1011 1111"]),
    row(44, "RET", "", NONE, &["1010 1111"]),
    row(44, "RETB", "", NONE, &["1001 1111"]),
    row(44, "RETI", "", NONE, &["1000 1111"]),
    row(44, "PUSH", "PSW", NONE, &["0010 0010"]),
    row(44, "PUSH", "rp", NONE, &["1011 0pp1"]),
    row(44, "POP", "PSW", NONE, &["0010 0011"]),
    row(44, "POP", "rp", NONE, &["1011 0pp0"]),
    row(44, "MOVW", "SP,#word", NONE, &["1110 1110", "0001 1100", "Low byte", "High byte"]),
    row(44, "MOVW", "SP,AX", NONE, &["1001 1001", "0001 1100"]),
    row(44, "MOVW", "AX,SP", NONE, &["1000 1001", "0001 1100"]),

    // Page 45: unconditional branch, conditional branch, CPU control.
    row(45, "BR", "!addr16", NONE, &["1001 1011", "Low addr", "High addr"]),
    row(45, "BR", "$addr16", NONE, &["1111 1010", "jdisp"]),
    row(45, "BR", "AX", NONE, &["0011 0001", "1001 1000"]),
    row(45, "BC", "$addr16", NONE, &["1000 1101", "jdisp"]),
    row(45, "BNC", "$addr16", NONE, &["1001 1101", "jdisp"]),
    row(45, "BZ", "$addr16", NONE, &["1010 1101", "jdisp"]),
    row(45, "BNZ", "$addr16", NONE, &["1011 1101", "jdisp"]),
    row(45, "BT", "saddr.bit,$addr16", NONE, &["1bbb 1100", "Saddr-offset", "jdisp"]),
    row(45, "BT", "sfr.bit,$addr16", NONE, &["0011 0001", "0bbb 0110", "Sfr-offset", "jdisp"]),
    row(45, "BT", "A.bit,$addr16", NONE, &["0011 0001", "0bbb 1110", "jdisp"]),
    row(45, "BT", "PSW.bit,$addr16", NONE, &["1bbb 1100", "0001 1110", "jdisp"]),
    row(45, "BT", "[HL].bit,$addr16", NONE, &["0011 0001", "1bbb 0110", "jdisp"]),
    row(45, "BF", "saddr.bit,$addr16", NONE, &["0011 0001", "0bbb 0011", "Saddr-offset", "jdisp"]),
    row(45, "BF", "sfr.bit,$addr16", NONE, &["0011 0001", "0bbb 0111", "Sfr-offset", "jdisp"]),
    row(45, "BF", "A.bit,$addr16", NONE, &["0011 0001", "0bbb 1111", "jdisp"]),
    row(45, "BF", "PSW.bit,$addr16", NONE, &["0011 0001", "0bbb 0011", "0001 1110", "jdisp"]),
    row(45, "BF", "[HL].bit,$addr16", NONE, &["0011 0001", "1bbb 0111", "jdisp"]),
    row(45, "BTCLR", "saddr.bit,$addr16", NONE, &["0011 0001", "0bbb 0001", "Saddr-offset", "jdisp"]),
    row(45, "BTCLR", "sfr.bit,$addr16", NONE, &["0011 0001", "0bbb 0101", "Sfr-offset", "jdisp"]),
    row(45, "BTCLR", "A.bit,$addr16", NONE, &["0011 0001", "0bbb 1101", "jdisp"]),
    row(45, "BTCLR", "PSW.bit,$addr16", NONE, &["0011 0001", "0bbb 0001", "0001 1110", "jdisp"]),
    row(45, "BTCLR", "[HL].bit,$addr16", NONE, &["0011 0001", "1bbb 0101", "jdisp"]),
    row(45, "DBNZ", "B,$addr16", NONE, &["1000 1011", "jdisp"]),
    row(45, "DBNZ", "C,$addr16", NONE, &["1000 1010", "jdisp"]),
    row(45, "DBNZ", "saddr,$addr16", NONE, &["0000 0100", "Saddr-offset", "jdisp"]),
    row(45, "SEL", "RBn", NONE, &["0110 0001", "11n1 n000"]),
    row(45, "NOP", "", NONE, &["0000 0000"]),
    row(45, "EI", "", NONE, &["0111 1010", "0001 1110"]),
    row(45, "DI", "", NONE, &["0111 1011", "0001 1110"]),
    row(45, "HALT", "", NONE, &["0111 0001", "0001 0000"]),
    row(45, "STOP", "", NONE, &["0111 0001", "0000 0000"]),
];
