//! The builtin attribute `#[global_allocator]`, written token to token as rustc's
//! `global_allocator` expansion writes it: the static as written, then beside it, at module
//! level, the four allocator symbols alloc declares (library/alloc/src/alloc.rs, the
//! `unsafe extern "Rust"` block), each calling the static's `GlobalAlloc` method.
//!
//! The signatures are the pinned library's declarations, exactly: `align` is
//! `core::mem::Alignment`, the pointer `__rust_dealloc` and `__rust_realloc` take is
//! `core::ptr::NonNull<u8>`, and the `Layout` is made by `Layout::from_size_alignment_unchecked`.
//! The binding side (blaze-ladder front2/alloc_shim.rs) finds each fn by its name, its
//! `#[rustc_std_internal_symbol]` and its body, in the static's crate, so the fns are at module
//! level (not in rustc's `const _: () = { .. }` block) and named exactly as the symbols.
//!
//! What the expansion writes is made def-site ([`Transparency::Opaque`]): its `$crate` is core,
//! where the macro is defined, and its parameter names cannot meet the user's. The static's name
//! is the item's own token ([`TokenOrigin::Input`]), so it names the static where it is written.

use crate::{
    error::ExpandError,
    expand::Out,
    expansion::{Diagnostic, TokenOrigin},
    tokens::{Delim, Raw, TokenKind, TokenStream, Transparency, index},
};

/// What the expansion writes: def-site.
const MADE: Transparency = Transparency::Opaque;

/// rustc's words for an item that is no static.
const NOT_STATIC: &str = "allocators must be statics";

/// rustc's words for `#[global_allocator(..)]` or `#[global_allocator = ..]`.
const TAKES_NO_ARGS: &str = "malformed `global_allocator` attribute input";

/// One of the four symbols. Its body calls `GlobalAlloc::<method>(&STATIC, ..)` with, in
/// order: the pointer as `*mut u8` (when `ptr`), the layout of `size` and `align`, then `rest`.
struct Symbol {
    name: &'static str,
    /// The parameters, with their parentheses.
    params: &'static str,
    /// The return type with its arrow; empty for none.
    ret: &'static str,
    method: &'static str,
    ptr: bool,
    /// The parameter that is the layout's size.
    size: &'static str,
    rest: &'static str,
}

/// alloc's four allocator declarations, in its order.
const SYMBOLS: [Symbol; 4] = [
    Symbol {
        name: "__rust_alloc",
        params: "(size: usize, align: $crate::mem::Alignment)",
        ret: "-> *mut u8",
        method: "alloc",
        ptr: false,
        size: "size",
        rest: "",
    },
    Symbol {
        name: "__rust_dealloc",
        params: "(ptr: $crate::ptr::NonNull<u8> , size: usize, align: $crate::mem::Alignment)",
        ret: "",
        method: "dealloc",
        ptr: true,
        size: "size",
        rest: "",
    },
    Symbol {
        name: "__rust_realloc",
        params: "(ptr: $crate::ptr::NonNull<u8> , old_size: usize, align: $crate::mem::Alignment, new_size: usize)",
        ret: "-> *mut u8",
        method: "realloc",
        ptr: true,
        size: "old_size",
        rest: ", new_size",
    },
    Symbol {
        name: "__rust_alloc_zeroed",
        params: "(size: usize, align: $crate::mem::Alignment)",
        ret: "-> *mut u8",
        method: "alloc_zeroed",
        ptr: false,
        size: "size",
        rest: "",
    },
];

/// Expands `#[global_allocator]` on the item in `out`.
pub(crate) fn expand(out: &mut Out<'_, '_>) -> Result<(), ExpandError> {
    let input = out.call.input();
    if out.call.attr_args().is_some() {
        return refuse(out, TAKES_NO_ARGS, None);
    }
    let Some(name) = static_name(input) else {
        let at = (!input.tokens().is_empty()).then_some(TokenOrigin::Input(0));
        return refuse(out, NOT_STATIC, at);
    };
    for (at, token) in input.tokens().iter().enumerate() {
        out.push(token.kind, input.text_of(token), TokenOrigin::Input(index(at)))?;
    }
    for symbol in &SYMBOLS {
        out.made_code("#[rustc_std_internal_symbol] unsafe fn", MADE)?;
        out.made_code(symbol.name, MADE)?;
        out.made_code(symbol.params, MADE)?;
        out.made_code(symbol.ret, MADE)?;
        out.made_code("{ unsafe { $crate::alloc::GlobalAlloc::", MADE)?;
        out.made_code(symbol.method, MADE)?;
        out.made_code("(&", MADE)?;
        let token = &input.tokens()[name];
        let ident = token.kind.with_spacing(crate::tokens::Spacing::Alone);
        out.push(ident, input.text_of(token), TokenOrigin::Input(index(name)))?;
        out.made_code(",", MADE)?;
        if symbol.ptr {
            out.made_code("$crate::ptr::NonNull::as_ptr(ptr),", MADE)?;
        }
        out.made_code("$crate::alloc::Layout::from_size_alignment_unchecked(", MADE)?;
        out.made_code(symbol.size, MADE)?;
        out.made_code(", align)", MADE)?;
        out.made_code(symbol.rest, MADE)?;
        out.made_code(") } }", MADE)?;
    }
    Ok(())
}

/// The index of the static's name in `item`: past outer attributes and a visibility, `static`,
/// an optional `mut`, the name, then `:`. `None` when the item is no static.
fn static_name(item: &TokenStream<'_>) -> Option<usize> {
    let tokens = item.tokens();
    let kind = |at: usize| tokens.get(at).map(|t| t.kind);
    let word = |at: usize, want: &str| {
        matches!(kind(at), Some(TokenKind::Ident { raw: Raw::No, .. })) && item.text_at(at) == want
    };
    let mut at = 0;
    // A `$item` a declarative macro passed on is wrapped in an invisible group.
    while matches!(kind(at), Some(TokenKind::Open(Delim::Invisible))) {
        at += 1;
    }
    while matches!(kind(at), Some(TokenKind::Punct { char: '#', .. }))
        && matches!(kind(at + 1), Some(TokenKind::Open(Delim::Bracket)))
    {
        at = item.close_of(at + 1)? + 1;
    }
    if word(at, "pub") {
        at += 1;
        if matches!(kind(at), Some(TokenKind::Open(Delim::Paren))) {
            at = item.close_of(at)? + 1;
        }
    }
    if !word(at, "static") {
        return None;
    }
    at += 1;
    if word(at, "mut") {
        at += 1;
    }
    let name = at;
    let named = matches!(kind(name), Some(TokenKind::Ident { .. }));
    let typed = matches!(kind(name + 1), Some(TokenKind::Punct { char: ':', .. }));
    (named && typed).then_some(name)
}

/// The attribute's error, `message`; it makes nothing else.
fn refuse(out: &mut Out<'_, '_>, message: &str, at: Option<TokenOrigin>) -> Result<(), ExpandError> {
    let message = out.note(message)?;
    out.diagnose(Diagnostic::Builtin { message, at });
    Ok(())
}
