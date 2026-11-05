//! What the syntax tree names but rustfmt never looks inside: the payloads of nodes that only
//! rustc's expansion or error recovery makes. The lowering builds only the error markers.

/// rustc's proof that an error was reported.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ErrorGuaranteed(());

impl ErrorGuaranteed {
    /// An error the parse has reported (or refused the file for), as rustc proves one.
    pub(crate) fn unchecked_error_guaranteed() -> ErrorGuaranteed {
        ErrorGuaranteed(())
    }
}

/// The bytes of a byte string literal, interned for good like a symbol's text.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ByteSymbol(pub &'static [u8]);

impl ByteSymbol {
    pub fn intern(bytes: &[u8]) -> ByteSymbol {
        ByteSymbol(Box::leak(bytes.to_vec().into_boxed_slice()))
    }

    pub fn as_byte_str(&self) -> &[u8] {
        self.0
    }
}

/// A `u128` without the alignment of one, as rustc packs it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pu128(pub u128);

impl Pu128 {
    pub fn get(self) -> u128 {
        self.0
    }
}

impl From<u128> for Pu128 {
    fn from(value: u128) -> Self {
        Pu128(value)
    }
}

impl std::fmt::Display for Pu128 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The expansion of `format_args!`, which only rustc's built-in macro produces.
#[derive(Clone, Debug)]
pub struct FormatArgs;

/// A `#[cfg]` predicate as rustc's attribute parser records it.
#[derive(Clone, Debug)]
pub struct CfgEntry;
