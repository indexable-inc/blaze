//! Byte emission.
//!
//! Every MSP430 instruction is a whole number of little-endian 16-bit words,
//! so an instruction is built as words and turned into bytes at the end.
//! Fixups are placed by word index, because the fields the MSP430X
//! relocations describe start at the extension word and reach forward over
//! the words between: see [`super::reloc`].

use super::operand::Expr;
use crate::section::{Fixup, FixupKind, Variant};

/// One instruction being built.
#[derive(Default)]
pub struct Enc {
    words: Vec<u16>,
    fixups: Vec<Fixup>,
}

impl Enc {
    pub fn new() -> Enc {
        Enc::default()
    }

    /// Appends a word, and returns its index.
    pub fn word(&mut self, w: u16) -> usize {
        self.words.push(w);
        self.words.len() - 1
    }

    /// Places a fixup over the field starting at word `at`. The words it
    /// covers must already be there, since the field is read back and
    /// rewritten by the fixup's own scatter function.
    pub fn fixup(&mut self, at: usize, x: Expr, kind: FixupKind) {
        debug_assert!(
            at * 2 + kind.size as usize <= self.words.len() * 2,
            "a fixup has to lie inside the instruction"
        );
        self.fixups.push(Fixup {
            offset: at as u32 * 2,
            expr: x.e,
            kind,
            span: x.span,
        });
    }

    /// The one candidate encoding: MSP430 instructions have a single size,
    /// so nothing here is ever relaxed.
    pub fn done(self) -> Option<Vec<Variant>> {
        let mut bytes = Vec::with_capacity(self.words.len() * 2);
        for w in &self.words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        Some(vec![Variant {
            bytes,
            fixups: self.fixups,
        }])
    }
}
