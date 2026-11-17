//! How a [`Symbol`] is named in LLVM IR, and back: the one convention the lowering (D2) writes
//! and the remark reader (D1) reads, so a remark's `Caller` and `Callee` are symbols again.
//!
//! An instance or static named by its symbol is `bz` followed by the symbol's 32 bytes as 64
//! lowercase hex digits. A C library name (`Named::C`) is written as itself, and read back as
//! `Symbol::of_name` of it: blaze-code's own symbol of a C name. A C name that is itself `bz` and
//! 64 lowercase hex digits would read back as that symbol; the lowering refuses such a C name
//! (`LowerError::CName`), so the two never meet.

use blaze_code::record::Symbol;

/// The bytes of an IR name: `bz` and 64 hex digits.
pub const IR_NAME_BYTES: usize = 2 + 64;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `symbol`'s IR name.
pub fn ir_name(symbol: Symbol) -> [u8; IR_NAME_BYTES] {
    let mut out = [0u8; IR_NAME_BYTES];
    out[0] = b'b';
    out[1] = b'z';
    for (i, byte) in symbol.0.iter().enumerate() {
        out[2 + 2 * i] = HEX[(byte >> 4) as usize];
        out[3 + 2 * i] = HEX[(byte & 15) as usize];
    }
    out
}

/// The symbol an IR name holds, when it has the `bz` form.
pub fn symbol_in(name: &[u8]) -> Option<Symbol> {
    if name.len() != IR_NAME_BYTES || &name[..2] != b"bz" {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = digit(name[2 + 2 * i])?;
        let lo = digit(name[3 + 2 * i])?;
        *slot = (hi << 4) | lo;
    }
    Some(Symbol(out))
}

/// The symbol of any function name LLVM gives back: the `bz` form's symbol, or a C name's.
pub fn symbol_of(name: &[u8]) -> Option<Symbol> {
    if let Some(symbol) = symbol_in(name) {
        return Some(symbol);
    }
    let text = core::str::from_utf8(name).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(Symbol::of_name(text))
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// `ir_name` as text (it is ASCII by construction).
pub fn ir_name_str(name: &[u8; IR_NAME_BYTES]) -> &str {
    // Every byte is `b`, `z` or a lowercase hex digit.
    core::str::from_utf8(name).unwrap_or("")
}
