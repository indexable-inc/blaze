//! The builder: how a const macro writes its output, as a flat tree and never as text.
//!
//! A [`Builder`] appends elements in preorder: [`Builder::open`] a node of a [`Kind`], write a
//! token ([`Builder::token`], [`Builder::token_at`]), splice a node or token of the input by
//! reference ([`Builder::node`], [`Builder::tok`], or `splice` on any view), and
//! [`Builder::close`]; [`Builder::finish`] gives the [`Built`] arrays. Sizes are relative (a
//! node's size counts its subtree from itself), so a run of elements a function appended means
//! the same wherever it lands, and a splice is one element naming the input's, so its tokens
//! keep their spans without the macro copying them.
//!
//! Nothing here checks that a node may hold what it is given: the host parses the output's
//! tokens and refuses a tree that is not their parse, naming the element, so the parser stays
//! the one home of the grammar.
//!
//! [`Builder::new`] grows on the const heap (`const_allocate`), which only blaze's const
//! evaluator has (`cfg(blaze_const_eval)`, the macro graph); [`Builder::over`] writes into
//! arrays its caller gives and runs anywhere, which is what the native tests use.

use crate::{Built, Entry, Kind, NO_SPAN, Node, SPLICE, Span, TOKEN, Token};

/// One `Kind` constant for every node kind of `kinds.rs`, in its order.
macro_rules! kinds {
    ($($name:ident)*) => {
        impl Kind {
            $(
                #[doc = concat!("`SyntaxKind::", stringify!($name), "`.")]
                pub const $name: Kind = Kind(crate::kinds::$name);
            )*
        }

        /// Every name `kinds!` was given, for the test that every node kind is public.
        #[cfg(test)]
        pub(crate) const KIND_NAMES: &[&str] = &[$(stringify!($name)),*];
    };
}

kinds!(
    ABI ARG_LIST ARRAY_EXPR ARRAY_TYPE ASM_CLOBBER_ABI ASM_CONST ASM_DIR_SPEC ASM_EXPR ASM_LABEL
    ASM_OPERAND_EXPR ASM_OPERAND_NAMED ASM_OPTION ASM_OPTIONS ASM_REG_OPERAND ASM_REG_SPEC ASM_SYM
    ASSOC_ITEM_LIST ASSOC_TYPE_ARG ATTR AWAIT_EXPR BECOME_EXPR BIN_EXPR BLOCK_EXPR BOX_PAT
    BREAK_EXPR CALL_EXPR CAST_EXPR CFG_ATOM CFG_ATTR_META CFG_COMPOSITE CFG_META CFG_PRED_EXPR
    CFG_PRED_PAT CLOSURE_EXPR CONST CONST_ARG CONST_BLOCK_PAT CONST_PARAM CONTINUE_EXPR DEREF_PAT
    DOC_COMMENT DYN_TRAIT_TYPE ENUM EXPR_STMT EXTERN_BLOCK EXTERN_CRATE EXTERN_ITEM_LIST FIELD_EXPR
    FN FN_PTR_TYPE FORMAT_ARGS_ARG FORMAT_ARGS_EXPR FOR_BINDER FOR_EXPR FOR_TYPE GENERIC_ARG_LIST
    GENERIC_PARAM_LIST GUARD_PAT IDENT_PAT IF_EXPR IMPL IMPL_RESTRICTION IMPL_TRAIT_TYPE
    INCLUDE_BYTES_EXPR INDEX_EXPR INFER_TYPE ITEM_LIST KEY_VALUE_META LABEL LET_ELSE LET_EXPR
    LET_STMT LIFETIME LIFETIME_ARG LIFETIME_PARAM LITERAL LITERAL_PAT LOOP_EXPR MACRO_CALL
    MACRO_DEF MACRO_EXPR MACRO_ITEMS MACRO_PAT MACRO_RULES MACRO_STMTS MACRO_TYPE MATCH_ARM
    MATCH_ARM_LIST MATCH_EXPR MATCH_GUARD METHOD_CALL_EXPR MODULE MUT_RESTRICTION NAME NAME_REF
    NEVER_TYPE NOT_NULL OFFSET_OF_EXPR OR_PAT PARAM PARAM_LIST PARENTHESIZED_ARG_LIST PAREN_EXPR
    PAREN_PAT PAREN_TYPE PATH PATH_EXPR PATH_META PATH_PAT PATH_SEGMENT PATH_TYPE PATTERN_TYPE
    PREFIX_EXPR PTR_TYPE RANGE_EXPR RANGE_PAT RECORD_EXPR RECORD_EXPR_FIELD RECORD_EXPR_FIELD_LIST
    RECORD_FIELD RECORD_FIELD_LIST RECORD_PAT RECORD_PAT_FIELD RECORD_PAT_FIELD_LIST REF_EXPR
    REF_PAT REF_TYPE RENAME REST_PAT RETURN_EXPR RETURN_TYPE_SYNTAX RET_TYPE SELF_PARAM SLICE_PAT
    SLICE_TYPE SOURCE_FILE STATIC STMT_LIST STRUCT TOKEN_TREE TOKEN_TREE_META TRAIT
    TRY_BLOCK_MODIFIER TRY_EXPR TUPLE_EXPR TUPLE_FIELD TUPLE_FIELD_LIST TUPLE_PAT TUPLE_STRUCT_PAT
    TUPLE_TYPE TYPE_ALIAS TYPE_ANCHOR TYPE_ARG TYPE_BOUND TYPE_BOUND_LIST TYPE_PARAM
    UNDERSCORE_EXPR UNION UNSAFE_META USE USE_BOUND_GENERIC_ARGS USE_TREE USE_TREE_LIST VARIANT
    VARIANT_LIST VISIBILITY VISIBILITY_INNER WHERE_CLAUSE WHERE_PRED WHILE_EXPR WILDCARD_PAT
    YEET_EXPR YIELD_EXPR UNSAFE_BINDER_CAST_EXPR UNSAFE_BINDER_TYPE SPEC_CLAUSE UNION_TYPE
    RECORD_TYPE LAZY_BODY
);

/// The capacity of [`Builder::new`]'s arrays when it first grows: a usual derive's whole
/// output, so it never copies.
const FIRST_CAPACITY: usize = 1024;

/// Writes a const macro's output tree: see the module.
///
/// `site` is the span [`Builder::token`] writes with: a builder is made with its site, so no
/// token gets one silently. [`Span::mixed_site`] is what a macro's own tokens should use.
pub struct Builder<'a> {
    kinds: &'a mut [u16],
    sizes: &'a mut [u32],
    spans: &'a mut [u32],
    texts: &'a mut [&'a str],
    len: u32,
    depth: u32,
    site: Span,
    grows: bool,
}

/// A node [`Builder::open`] opened, for [`Builder::close`]: its element and how deep it is.
#[must_use]
#[derive(Clone, Copy)]
pub struct Open {
    at: u32,
    depth: u32,
}

impl Builder<'static> {
    /// An empty builder that grows on the const heap: 1,024 elements the first time, then
    /// double.
    ///
    /// # Panics
    ///
    /// Outside blaze's const evaluator, the first time it must grow (use [`Builder::over`]).
    pub const fn new(site: Span) -> Builder<'static> {
        Builder {
            kinds: heap::u16s(heap::NEW_CAPACITY),
            sizes: heap::u32s(heap::NEW_CAPACITY),
            spans: heap::u32s(heap::NEW_CAPACITY),
            texts: heap::strs(heap::NEW_CAPACITY),
            len: 0,
            depth: 0,
            site,
            grows: true,
        }
    }

    /// A new text on the const heap, the parts one after another: a name a macro makes
    /// (`concat(&["get_", name.text()])`).
    ///
    /// # Panics
    ///
    /// Outside blaze's const evaluator.
    pub const fn concat(&mut self, parts: &[&str]) -> &'static str {
        heap::str(parts)
    }

    /// `n` in decimal, a new text on the const heap: an integer literal's text.
    ///
    /// # Panics
    ///
    /// Outside blaze's const evaluator.
    pub const fn decimal(&mut self, n: u128) -> &'static str {
        heap::decimal(n)
    }
}

impl<'a> Builder<'a> {
    /// A builder that writes into these arrays, which are as long as each other, and never
    /// grows.
    ///
    /// # Panics
    ///
    /// When the arrays are not as long as each other; later, when an element would be one
    /// more than they hold ("the output has more than N elements").
    pub const fn over(
        kinds: &'a mut [u16],
        sizes: &'a mut [u32],
        spans: &'a mut [u32],
        texts: &'a mut [&'a str],
        site: Span,
    ) -> Builder<'a> {
        let len = kinds.len();
        if sizes.len() != len || spans.len() != len || texts.len() != len {
            panic!("const_ast::Builder::over: the four arrays are as long as each other");
        }
        Builder { kinds, sizes, spans, texts, len: 0, depth: 0, site, grows: false }
    }

    /// The span [`Builder::token`] writes with.
    pub const fn site(&self) -> Span {
        self.site
    }

    /// Makes `site` the span [`Builder::token`] writes with, and gives the one before.
    pub const fn set_site(&mut self, site: Span) -> Span {
        let before = self.site;
        self.site = site;
        before
    }

    /// Opens a node of `kind`: what is written until its [`Builder::close`] is its subtree.
    pub const fn open(&mut self, kind: Kind) -> Open {
        let at = self.push(kind.0, NO_SPAN, "");
        let Some(depth) = self.depth.checked_add(1) else {
            panic!("const_ast::Builder::open: nesting depth overflow");
        };
        self.depth = depth;
        Open { at, depth }
    }

    /// Closes the node `open` opened.
    ///
    /// # Panics
    ///
    /// When it is not the innermost open node.
    pub const fn close(&mut self, open: Open) {
        if open.depth != self.depth || open.at >= self.len || self.sizes[open.at as usize] != 0 {
            panic!("const_ast::Builder::close: the node is not the innermost open one");
        }
        self.sizes[open.at as usize] = self.len - open.at;
        self.depth -= 1;
    }

    /// Writes a token of `text` with the builder's site.
    ///
    /// # Panics
    ///
    /// As [`Builder::token_at`].
    pub const fn token(&mut self, text: &'a str) {
        let site = self.site;
        self.token_at(text, site)
    }

    /// Writes a token of `text` with the span `span`. Its kind is the host's to find, by
    /// lexing the text alone.
    ///
    /// # Panics
    ///
    /// When `text` is empty.
    pub const fn token_at(&mut self, text: &'a str, span: Span) {
        let bytes = text.as_bytes().len();
        if bytes == 0 {
            panic!("const_ast::Builder::token_at: a token's text is not empty");
        }
        self.push(TOKEN, span.0, text);
    }

    /// Splices `node` of an input: one element, which the host replaces with the node's
    /// subtree, spans and all.
    pub const fn node(&mut self, node: Node<'_>) {
        self.splice(node.tree().input, node.index());
    }

    /// Splices `token` of an input: one element, which the host replaces with the token and
    /// its span.
    pub const fn tok(&mut self, token: Token<'_>) {
        self.splice(token.tree.input, token.index());
    }

    /// How many elements it has written.
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether it has written none.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The output: the elements written, to be parsed as `entry`.
    ///
    /// # Panics
    ///
    /// While a node is open.
    pub const fn finish(self, entry: Entry) -> Built<'a> {
        if self.depth != 0 {
            panic!("const_ast::Builder::finish: a node is still open");
        }
        let len = self.len as usize;
        let Builder { kinds, sizes, spans, texts, .. } = self;
        Built {
            kinds: heap::prefix(kinds, len),
            sizes: heap::prefix(sizes, len),
            spans: heap::prefix(spans, len),
            texts: heap::prefix(texts, len),
            entry,
        }
    }

    /// Writes `token` (a splice of an input's) inside the nodes `wrap` names, outer to inner:
    /// a name put where a path goes is `[PATH_TYPE, PATH, PATH_SEGMENT, NAME_REF]` around it.
    pub(crate) const fn tok_in(&mut self, token: Token<'_>, wrap: &[Kind]) {
        let input = token.tree.input;
        if input >= SPLICE as u32 {
            panic!("const_ast::Builder: a splice names an input below 16384");
        }
        self.push_wrapped(wrap, SPLICE | input as u16, token.index(), "");
    }

    /// Writes a token of `text` with `span` inside the nodes `wrap` names, outer to inner.
    pub(crate) const fn token_in(&mut self, text: &'a str, span: Span, wrap: &[Kind]) {
        if text.as_bytes().is_empty() {
            panic!("const_ast::Builder::token_at: a token's text is not empty");
        }
        self.push_wrapped(wrap, TOKEN, span.0, text);
    }

    /// Writes `wrap` nodes (outer to inner) around one leaf element with one capacity check and
    /// precomputed subtree sizes, avoiding redundant per-wrapper grow/close checks.
    const fn push_wrapped(&mut self, wrap: &[Kind], leaf_kind: u16, leaf_span: u32, leaf_text: &'a str) {
        let n = wrap.len();
        let Some(total) = n.checked_add(1) else {
            panic!("const_ast::Builder: output length overflow");
        };
        let Some(end) = (self.len as usize).checked_add(total) else {
            panic!("const_ast::Builder: output length overflow");
        };
        if end > u32::MAX as usize {
            panic!("const_ast::Builder: output length overflow");
        }
        if end > self.kinds.len() {
            self.grow_to(end);
        }
        let start = self.len as usize;
        let mut i = 0;
        while i < n {
            let idx = start + i;
            self.kinds[idx] = wrap[i].0;
            self.sizes[idx] = (total - i) as u32;
            self.spans[idx] = NO_SPAN;
            self.texts[idx] = "";
            i += 1;
        }
        let leaf = start + n;
        self.kinds[leaf] = leaf_kind;
        self.sizes[leaf] = 1;
        self.spans[leaf] = leaf_span;
        self.texts[leaf] = leaf_text;
        self.len = end as u32;
    }

    /// Writes a splice of element `at` of input `input`.
    const fn splice(&mut self, input: u32, at: u32) {
        if input >= SPLICE as u32 {
            panic!("const_ast::Builder: a splice names an input below 16384");
        }
        self.push(SPLICE | input as u16, at, "");
    }

    /// Appends an element of `kind` with `span` and `text`, a size of 1 for a token or a
    /// splice and 0 (open) for a node, and gives its number.
    const fn push(&mut self, kind: u16, span: u32, text: &'a str) -> u32 {
        let Some(next_len) = self.len.checked_add(1) else {
            panic!("const_ast::Builder: output length overflow");
        };
        if (self.len as usize) == self.kinds.len() {
            self.grow_to(next_len as usize);
        }
        let at = self.len;
        let i = at as usize;
        self.kinds[i] = kind;
        self.sizes[i] = if kind & (TOKEN | SPLICE) != 0 { 1 } else { 0 };
        self.spans[i] = span;
        self.texts[i] = text;
        self.len = next_len;
        at
    }

    /// Makes room for at least `min_capacity` elements: doubles a growing builder's arrays
    /// (the first time to [`FIRST_CAPACITY`]) on the const heap, copying what is written.
    ///
    /// # Panics
    ///
    /// A builder [`Builder::over`] made: "the output has more than N elements".
    const fn grow_to(&mut self, min_capacity: usize) {
        let len = self.kinds.len();
        if !self.grows {
            full(len);
        }
        let mut capacity = if len == 0 { FIRST_CAPACITY } else { len };
        while capacity < min_capacity {
            capacity = match capacity.checked_mul(2) {
                Some(c) if c <= u32::MAX as usize => c,
                _ => {
                    if min_capacity <= u32::MAX as usize {
                        min_capacity
                    } else {
                        panic!("const_ast::Builder: capacity overflow");
                    }
                }
            };
        }
        let kinds = heap::u16s(capacity);
        let sizes = heap::u32s(capacity);
        let spans = heap::u32s(capacity);
        let texts = heap::strs(capacity);
        let mut i = 0;
        while i < len {
            kinds[i] = self.kinds[i];
            sizes[i] = self.sizes[i];
            spans[i] = self.spans[i];
            texts[i] = self.texts[i];
            i += 1;
        }
        self.kinds = kinds;
        self.sizes = sizes;
        self.spans = spans;
        self.texts = texts;
    }
}

/// A text with its span, made before the builder writes it: what `quote!` splices for a name
/// a macro made ([`Builder::concat`]).
#[derive(Clone, Copy)]
pub struct Text {
    text: &'static str,
    span: Span,
}

/// `text` with `span`, to splice as a token.
pub const fn text(text: &'static str, span: Span) -> Text {
    Text { text, span }
}

impl Text {
    /// Writes it as a token, inside the nodes `wrap` names, outer to inner.
    pub const fn splice(self, b: &mut Builder<'static>, wrap: &[Kind]) {
        b.token_in(self.text, self.span, wrap);
    }
}

/// Formats `n` in decimal into the tail of a 39-byte buffer in one pass, returning `(buf, start)`
/// so `buf[start..]` holds the ASCII decimal digits.
pub(crate) const fn decimal_bytes(mut n: u128) -> ([u8; 39], usize) {
    let mut buf = [0u8; 39];
    let mut at = 39;
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return (buf, at);
        }
    }
}

/// Panics for a builder of `capacity` elements that was given one more.
const fn full(capacity: usize) -> ! {
    heap::panic_with_number(
        "const_ast::Builder: the output has more than ",
        capacity as u128,
        " elements",
    )
}

/// The const heap a growing builder writes into: `const_allocate`, the nightly's, which blaze's
/// const evaluator answers (`Intrinsic::ConstAllocate`). Each slice is made by transmuting a
/// pointer and a length, so no pointer becomes an integer.
#[cfg(blaze_const_eval)]
#[expect(unsafe_code, reason = "the const heap: const_allocate and slices from its pointers")]
mod heap {
    #[rustc_intrinsic]
    const unsafe fn const_allocate(size: usize, align: usize) -> *mut u8;

    #[rustc_intrinsic]
    const fn size_of<T>() -> usize;

    #[rustc_intrinsic]
    const fn align_of<T>() -> usize;

    /// A slice's two words, `repr(C)` as rustc lays out `&[T]` and `&str`: the pointer, then
    /// the length.
    #[repr(C)]
    struct Raw {
        ptr: *mut u8,
        len: usize,
    }

    /// What [`crate::Builder::new`] starts with: grown at once to its first capacity, so the
    /// first element does not copy.
    pub(crate) const NEW_CAPACITY: usize = super::FIRST_CAPACITY;

    /// `len` elements of `T` on the const heap.
    ///
    /// The invariant: an element is written before it is read. The builder reads element `i`
    /// only below its `len`, and writes each one when it pushes it (`Builder::push`) or copies
    /// it while growing, so no uninitialized element is read.
    const fn alloc<T>(len: usize) -> Raw {
        let Some(bytes) = len.checked_mul(size_of::<T>()) else {
            panic!("const_ast::Builder: allocation size overflow");
        };
        // SAFETY: `const_allocate` has no precondition beyond a power-of-two `align`, which
        // `align_of` is; the block lives as long as the evaluation, which is `'static` to the
        // macro.
        let ptr = unsafe { const_allocate(bytes, align_of::<T>()) };
        Raw { ptr, len }
    }

    pub(crate) const fn u16s(len: usize) -> &'static mut [u16] {
        let raw = alloc::<u16>(len);
        // SAFETY: `raw` is a fresh block of `len` `u16`s, aligned for `u16`, that nothing else
        // points to; `alloc`'s invariant is why its uninitialized elements are never read.
        unsafe { core::mem::transmute::<Raw, &'static mut [u16]>(raw) }
    }

    pub(crate) const fn u32s(len: usize) -> &'static mut [u32] {
        let raw = alloc::<u32>(len);
        // SAFETY: as `u16s`, for `u32`.
        unsafe { core::mem::transmute::<Raw, &'static mut [u32]>(raw) }
    }

    /// `len` texts, of any lifetime: the builder's arrays are `&'a mut [&'a str]`, which a
    /// `&'static mut [&'static str]` is not (`&mut` is invariant in what it points to).
    pub(crate) const fn strs<'t>(len: usize) -> &'t mut [&'t str] {
        let raw = alloc::<&'t str>(len);
        // SAFETY: as `u16s`, for `&str`; an element is a text the builder was given, written
        // before it is read. The block lives for the whole evaluation, so for any `'t`.
        unsafe { core::mem::transmute::<Raw, &'t mut [&'t str]>(raw) }
    }

    /// The parts one after another, a new text on the const heap.
    pub(crate) const fn str(parts: &[&str]) -> &'static str {
        let mut total = 0usize;
        let mut i = 0;
        while i < parts.len() {
            total = match total.checked_add(parts[i].as_bytes().len()) {
                Some(t) => t,
                None => panic!("const_ast::Builder::concat: string length overflow"),
            };
            i += 1;
        }
        let raw = alloc::<u8>(total);
        let (ptr, len) = (raw.ptr, raw.len);
        // SAFETY: as `u16s`, for `u8`; every byte is written below before the text is made.
        let bytes = unsafe { core::mem::transmute::<Raw, &'static mut [u8]>(raw) };
        let mut at = 0;
        let mut i = 0;
        while i < parts.len() {
            let part = parts[i].as_bytes();
            let mut j = 0;
            while j < part.len() {
                bytes[at] = part[j];
                at += 1;
                j += 1;
            }
            i += 1;
        }
        let raw = Raw { ptr, len };
        // SAFETY: the `len` bytes at `raw.ptr` are the parts' bytes in order, each part UTF-8,
        // so they are UTF-8; nothing writes them after this.
        unsafe { core::mem::transmute::<Raw, &'static str>(raw) }
    }

    /// Formats `n` in decimal directly into one `[u8; 39]` stack buffer and copies the active
    /// suffix to the const heap in a single pass.
    pub(crate) const fn decimal(n: u128) -> &'static str {
        let (buf, start) = super::decimal_bytes(n);
        let len = buf.len() - start;
        let raw = alloc::<u8>(len);
        let ptr = raw.ptr;
        // SAFETY: as `u16s`, for `u8`; every byte is written below before the text is made.
        let out = unsafe { core::mem::transmute::<Raw, &'static mut [u8]>(raw) };
        let mut i = 0;
        while i < len {
            out[i] = buf[start + i];
            i += 1;
        }
        // SAFETY: `out` holds ASCII digits `'0'..='9'`, which are valid UTF-8; nothing writes
        // them after this.
        unsafe { core::mem::transmute::<Raw, &'static str>(Raw { ptr, len }) }
    }

    /// The first `len` elements of `slice`.
    pub(crate) const fn prefix<'a, T>(slice: &'a [T], len: usize) -> &'a [T] {
        if len > slice.len() {
            panic!("const_ast::Builder: a prefix longer than its array");
        }
        // SAFETY: `&[T]` is laid out as `Raw` (a pointer then a length).
        let raw = unsafe { core::mem::transmute::<&'a [T], Raw>(slice) };
        let raw = Raw { ptr: raw.ptr, len };
        // SAFETY: the first `len` elements of a slice, `len` checked against its length above,
        // borrowed for the slice's lifetime.
        unsafe { core::mem::transmute::<Raw, &'a [T]>(raw) }
    }

    /// Panics with `before`, `n` in decimal and `after`.
    pub(crate) const fn panic_with_number(before: &str, n: u128, after: &str) -> ! {
        let number = decimal(n);
        panic!("{}", str(&[before, number, after]))
    }
}

/// Outside blaze's const evaluator there is no const heap: [`Builder::new`] starts empty and
/// panics when it must grow, and [`Builder::over`] is the builder.
#[cfg(not(blaze_const_eval))]
mod heap {
    /// What [`crate::Builder::new`] starts with: nothing, so it panics at its first element.
    pub(crate) const NEW_CAPACITY: usize = 0;

    macro_rules! no_heap {
        () => {
            panic!(
                "const_ast::Builder::new grows only in blaze's const evaluator; use Builder::over"
            )
        };
    }

    pub(crate) const fn u16s(len: usize) -> &'static mut [u16] {
        if len != 0 {
            no_heap!();
        }
        &mut []
    }

    pub(crate) const fn u32s(len: usize) -> &'static mut [u32] {
        if len != 0 {
            no_heap!();
        }
        &mut []
    }

    pub(crate) const fn strs<'t>(len: usize) -> &'t mut [&'t str] {
        if len != 0 {
            no_heap!();
        }
        &mut []
    }

    pub(crate) const fn str(_parts: &[&str]) -> &'static str {
        no_heap!()
    }

    pub(crate) const fn decimal(_n: u128) -> &'static str {
        no_heap!()
    }

    /// The first `len` elements of `slice`.
    pub(crate) const fn prefix<T>(slice: &[T], len: usize) -> &[T] {
        slice.split_at(len).0
    }

    /// Panics with `before`, `n` in decimal and `after`, the message made on the stack.
    pub(crate) const fn panic_with_number(before: &str, n: u128, after: &str) -> ! {
        let mut buf = [0u8; 160];
        let mut at = 0;
        at = put(&mut buf, at, before.as_bytes());
        let (digits, start) = super::decimal_bytes(n);
        at = put(&mut buf, at, digits.split_at(start).1);
        at = put(&mut buf, at, after.as_bytes());
        match core::str::from_utf8(buf.split_at(at).0) {
            Ok(message) => panic!("{}", message),
            Err(_) => panic!("const_ast::Builder: a panic message that is not UTF-8"),
        }
    }

    /// Copies `bytes` into `buf` from `at` (as many as fit) and gives where it stopped.
    const fn put(buf: &mut [u8], mut at: usize, bytes: &[u8]) -> usize {
        let mut i = 0;
        while i < bytes.len() && at < buf.len() {
            buf[at] = bytes[i];
            at += 1;
            i += 1;
        }
        at
    }
}
