//! A simplified version of quote-crate like quasi quote macro

use intern::{Symbol, sym};
use span::Span;
use tt::IdentIsRaw;

use crate::{name::Name, tt::TopSubtreeBuilder};

pub(crate) fn dollar_crate(span: Span) -> tt::Ident {
    tt::Ident { sym: sym::dollar_crate, span, is_raw: tt::IdentIsRaw::No }
}

// A helper macro quote macro
// FIXME:
// 1. Not all puncts are handled
// 2. #()* pattern repetition not supported now
//    * But we can do it manually, see `test_quote_derive_copy_hack`
#[doc(hidden)]
#[macro_export]
macro_rules! quote_impl__ {
    ($span:ident $builder:ident) => {};

    ( @SUBTREE($span:ident $builder:ident) $delim:ident $($tt:tt)* ) => {
        {
            $builder.open($crate::tt::DelimiterKind::$delim, $span);
            $crate::builtin::quote::__quote!($span $builder  $($tt)*);
            $builder.close($span);
        }
    };

    ( @PUNCT($span:ident $builder:ident) $first:literal ) => {
        $builder.push(
            $crate::tt::Leaf::Punct($crate::tt::Punct {
                char: $first,
                spacing: $crate::tt::Spacing::Alone,
                span: $span,
            })
        );
    };

    ( @PUNCT($span:ident $builder:ident) $first:literal, $sec:literal ) => {
        $builder.extend([
            $crate::tt::Leaf::Punct($crate::tt::Punct {
                char: $first,
                spacing: $crate::tt::Spacing::Joint,
                span: $span,
            }),
            $crate::tt::Leaf::Punct($crate::tt::Punct {
                char: $sec,
                spacing: $crate::tt::Spacing::Alone,
                span: $span,
            })
        ]);
    };

    // hash variable, taken by reference: a spliced `TopSubtree` or `String` is read, not cloned
    ($span:ident $builder:ident # $first:ident $($tail:tt)* ) => {
        $crate::builtin::quote::ToTokenTree::to_tokens(&$first, $span, $builder);
        $crate::builtin::quote::__quote!($span $builder $($tail)*);
    };

    ($span:ident $builder:ident # # $first:ident $($tail:tt)* ) => {{
        ::std::iter::IntoIterator::into_iter($first).for_each(|it| $crate::builtin::quote::ToTokenTree::to_tokens(&it, $span, $builder));
        $crate::builtin::quote::__quote!($span $builder $($tail)*);
    }};

    // Brace
    ($span:ident $builder:ident { $($tt:tt)* } ) => { $crate::builtin::quote::__quote!(@SUBTREE($span $builder) Brace $($tt)*) };
    // Bracket
    ($span:ident $builder:ident [ $($tt:tt)* ] ) => { $crate::builtin::quote::__quote!(@SUBTREE($span $builder) Bracket $($tt)*) };
    // Parenthesis
    ($span:ident $builder:ident ( $($tt:tt)* ) ) => { $crate::builtin::quote::__quote!(@SUBTREE($span $builder) Parenthesis $($tt)*) };

    // Literal
    ($span:ident $builder:ident $tt:literal ) => { $crate::builtin::quote::ToTokenTree::to_tokens($tt, $span, $builder) };
    // Ident: interned once per call site, then one acquire load per expansion.
    ($span:ident $builder:ident $tt:ident ) => {
        $builder.push(
            $crate::tt::Leaf::Ident($crate::tt::Ident {
                sym: {
                    static SYMBOL: ::std::sync::OnceLock<intern::Symbol> = ::std::sync::OnceLock::new();
                    *SYMBOL.get_or_init(|| intern::Symbol::intern(stringify!($tt)))
                },
                span: $span,
                is_raw: tt::IdentIsRaw::No,
            })
        );
    };

    // Puncts
    // FIXME: Not all puncts are handled
    ($span:ident $builder:ident -> ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '-', '>')};
    ($span:ident $builder:ident => ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '=', '>')};
    ($span:ident $builder:ident & ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '&')};
    ($span:ident $builder:ident , ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) ',')};
    ($span:ident $builder:ident : ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) ':')};
    ($span:ident $builder:ident ; ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) ';')};
    ($span:ident $builder:ident :: ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) ':', ':')};
    ($span:ident $builder:ident . ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '.')};
    ($span:ident $builder:ident < ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '<')};
    ($span:ident $builder:ident > ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '>')};
    ($span:ident $builder:ident ! ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '!')};
    ($span:ident $builder:ident # ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '#')};
    ($span:ident $builder:ident $ ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '$')};
    ($span:ident $builder:ident * ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '*')};
    ($span:ident $builder:ident = ) => {$crate::builtin::quote::__quote!(@PUNCT($span $builder) '=')};

    ($span:ident $builder:ident $first:tt $($tail:tt)+ ) => {{
        $crate::builtin::quote::__quote!($span $builder $first);
        $crate::builtin::quote::__quote!($span $builder $($tail)*);
    }};
}
pub use quote_impl__ as __quote;

/// FIXME:
/// It probably should implement in proc-macro
#[macro_export]
macro_rules! quote {
    ($span:ident=> $($tt:tt)* ) => {
        {
            let mut builder = $crate::tt::TopSubtreeBuilder::new($crate::tt::Delimiter {
                kind: $crate::tt::DelimiterKind::Invisible,
                open: $span,
                close: $span,
            });
            #[allow(unused)]
            let builder_ref = &mut builder;
            $crate::builtin::quote::__quote!($span builder_ref $($tt)*);
            builder.build_skip_top_subtree()
        }
    }
}
pub use quote;

/// [`quote!`], the tree built in `frame`: an expansion, which its query parses and drops.
#[macro_export]
macro_rules! quote_in {
    ($frame:expr, $span:ident=> $($tt:tt)* ) => {
        {
            let mut builder = $crate::tt::TopSubtreeBuilder::new($crate::tt::Delimiter {
                kind: $crate::tt::DelimiterKind::Invisible,
                open: $span,
                close: $span,
            });
            #[allow(unused)]
            let builder_ref = &mut builder;
            $crate::builtin::quote::__quote!($span builder_ref $($tt)*);
            builder.build_skip_top_subtree_in($frame)
        }
    }
}
pub use quote_in;

/// [`quote!`] written into a builder of the caller's (an expression of type
/// `&mut TopSubtreeBuilder`), with no top subtree of its own: one buffer for a whole expansion.
#[macro_export]
macro_rules! quote_into {
    ($builder:expr, $span:ident => $($tt:tt)* ) => {
        {
            #[allow(unused)]
            let builder_ref: &mut $crate::tt::TopSubtreeBuilder = $builder;
            $crate::builtin::quote::__quote!($span builder_ref $($tt)*);
        }
    }
}
pub use quote_into;

pub trait ToTokenTree {
    fn to_tokens(self, span: Span, builder: &mut TopSubtreeBuilder);
}

/// Wraps `TokenTreesView` with a delimiter (a subtree, but without allocating).
#[derive(Clone, Copy)]
pub struct WithDelimiter<'a> {
    pub delimiter: crate::tt::Delimiter,
    pub token_trees: crate::tt::TokenTreesView<'a>,
}

impl ToTokenTree for WithDelimiter<'_> {
    fn to_tokens(self, span: Span, builder: &mut TopSubtreeBuilder) {
        builder.open(self.delimiter.kind, self.delimiter.open);
        self.token_trees.to_tokens(span, builder);
        builder.close(self.delimiter.close);
    }
}

/// Spliced by `#it`: the tokens the closure writes, as they are.
#[derive(Clone, Copy)]
pub struct Emit<F>(pub F);

impl<F: Fn(&mut TopSubtreeBuilder)> ToTokenTree for &Emit<F> {
    #[inline]
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        (self.0)(builder)
    }
}

/// Spliced by `#it`: the tokens the closure writes in an invisible group at the span, which is
/// what splicing a [`quote!`] at that span of anything but one delimited group adds.
#[derive(Clone, Copy)]
pub struct Group<F>(pub Span, pub F);

impl<F: Fn(&mut TopSubtreeBuilder)> ToTokenTree for &Group<F> {
    #[inline]
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        builder.open(crate::tt::DelimiterKind::Invisible, self.0);
        (self.1)(builder);
        builder.close(self.0);
    }
}

#[inline]
pub fn emit<F: Fn(&mut TopSubtreeBuilder)>(f: F) -> Emit<F> {
    Emit(f)
}

#[inline]
pub fn group<F: Fn(&mut TopSubtreeBuilder)>(span: Span, f: F) -> Group<F> {
    Group(span, f)
}

impl ToTokenTree for crate::tt::TokenTreesView<'_> {
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        builder.extend_with_tt(self);
    }
}

impl ToTokenTree for crate::tt::SubtreeView<'_> {
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        builder.extend_with_tt(self.as_token_trees());
    }
}

impl<A: core::alloc::Allocator> ToTokenTree for &crate::tt::TopSubtree<A> {
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        builder.extend_with_tt(self.as_token_trees());
    }
}

impl ToTokenTree for crate::tt::TtElement<'_> {
    fn to_tokens(self, _: Span, builder: &mut TopSubtreeBuilder) {
        match self {
            crate::tt::TtElement::Leaf(leaf) => builder.push(leaf.clone()), // clone: a copy (symbol and span)
            crate::tt::TtElement::Subtree(subtree, subtree_iter) => {
                builder.open(subtree.delimiter.kind, subtree.delimiter.open);
                builder.extend_with_tt(subtree_iter.remaining());
                builder.close(subtree.delimiter.close);
            }
        }
    }
}

macro_rules! impl_to_to_tokentrees {
    ($($span:ident: $ty:ty => $this:ident $im:block;)*) => {
        $(
            impl ToTokenTree for $ty {
                fn to_tokens($this, $span: Span, builder: &mut TopSubtreeBuilder) {
                    let leaf: $crate::tt::Leaf = $im.into();
                    builder.push(leaf);
                }
            }
        )*
    }
}
/// Cheap to clone: a leaf, a number, a `Copy` handle. Owned buffers (`TopSubtree`, `String`)
/// implement the trait for their reference instead, so this never copies one.
impl<T: ToTokenTree + Clone> ToTokenTree for &T {
    fn to_tokens(self, span: Span, builder: &mut TopSubtreeBuilder) {
        self.clone().to_tokens(span, builder); // clone: a leaf, a number or a `Copy` handle, as the doc above says
    }
}

impl_to_to_tokentrees! {
    span: u32 => self { crate::tt::Literal{text_and_suffix: sym::Integer::get(self as _), span, kind: tt::LitKind::Integer, suffix_len: 0 } };
    span: usize => self { crate::tt::Literal{text_and_suffix: sym::Integer::get(self as _), span, kind: tt::LitKind::Integer, suffix_len: 0 } };
    span: i32 => self { crate::tt::Literal{text_and_suffix: sym::Integer::get(self as _), span, kind: tt::LitKind::Integer, suffix_len: 0 } };
    span: bool => self { crate::tt::Ident{sym: if self { sym::true_ } else { sym::false_ }, span, is_raw: tt::IdentIsRaw::No } };
    _span: crate::tt::Leaf => self { self };
    _span: crate::tt::Literal => self { self };
    _span: crate::tt::Ident => self { self };
    _span: crate::tt::Punct => self { self };
    span: &str => self { str_literal(self, span) };
    span: &String => self { str_literal(self, span) };
    span: Name => self {
        // A `Name` is never stored with `r#`.
        crate::tt::Ident{sym: *self.symbol(), span, is_raw: IdentIsRaw::No }
    };
    span: Symbol => self {
        match self.as_str().strip_prefix("r#") {
            Some(s) => crate::tt::Ident{sym: Symbol::intern(s), span, is_raw: IdentIsRaw::Yes },
            // Interning a symbol's own text gives the symbol back.
            None => crate::tt::Ident{sym: self, span, is_raw: IdentIsRaw::No },
        }
    };
}

/// Whether [`str::escape_default`] keeps the byte as it is: the printable ASCII other than quotes
/// and backslashes.
#[inline]
fn kept_by_escape_default(b: u8) -> bool {
    matches!(b, 0x20..=0x7e) && !matches!(b, b'"' | b'\'' | b'\\')
}

/// A string literal of `s`, escaped by [`str::escape_default`]: the runs it keeps are copied
/// whole, and only the characters between them escaped one by one.
pub(crate) fn str_literal(s: &str, span: Span) -> crate::tt::Literal {
    let text_and_suffix = match s.bytes().position(|b| !kept_by_escape_default(b)) {
        None => Symbol::intern(s),
        Some(first) => {
            let mut escaped = String::with_capacity(s.len() + s.len() / 2 + 2);
            // A kept byte is ASCII, so each run ends at the start of a character.
            let (kept, mut rest) = s.split_at(first);
            escaped.push_str(kept);
            while let Some(c) = rest.chars().next() {
                escaped.extend(c.escape_default());
                rest = &rest[c.len_utf8()..];
                let run = rest.bytes().position(|b| !kept_by_escape_default(b)).unwrap_or(rest.len());
                let (kept, tail) = rest.split_at(run);
                escaped.push_str(kept);
                rest = tail;
            }
            Symbol::intern(&escaped)
        }
    };
    crate::tt::Literal { text_and_suffix, span, kind: tt::LitKind::Str, suffix_len: 0 }
}

#[cfg(test)]
mod tests {
    use crate::tt;
    use ::tt::IdentIsRaw;
    use expect_test::expect;
    use intern::Symbol;
    use span::{Edition, ROOT_ERASED_FILE_AST_ID, SpanAnchor, SyntaxContext};
    use syntax::{TextRange, TextSize};

    use super::{Emit, Group};
    use crate::name::Name;

    const DUMMY: tt::Span = tt::Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor {
            file_id: span::EditionedFileId::new(
                span::FileId::from_raw(0xe4e4e),
                span::Edition::CURRENT,
            ),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    };

    const OTHER: tt::Span = tt::Span { range: TextRange::empty(TextSize::new(7)), ..DUMMY };

    #[test]
    fn test_quote_delimiters() {
        assert_eq!(quote!(DUMMY =>{}).to_string(), "{}");
        assert_eq!(quote!(DUMMY =>()).to_string(), "()");
        assert_eq!(quote!(DUMMY =>[]).to_string(), "[]");
    }

    #[test]
    fn test_quote_idents() {
        assert_eq!(quote!(DUMMY =>32).to_string(), "32");
        assert_eq!(quote!(DUMMY =>struct).to_string(), "struct");
    }

    #[test]
    fn test_quote_hash_simple_literal() {
        let a = 20;
        assert_eq!(quote!(DUMMY =>#a).to_string(), "20");
        let s: String = "hello".into();
        assert_eq!(quote!(DUMMY =>#s).to_string(), "\"hello\"");
    }

    fn mk_ident(name: &str) -> crate::tt::Ident {
        let (is_raw, s) = IdentIsRaw::split_from_symbol(name);
        crate::tt::Ident { sym: Symbol::intern(s), span: DUMMY, is_raw }
    }

    #[test]
    fn test_quote_hash_token_tree() {
        let a = mk_ident("hello");

        let quoted = quote!(DUMMY =>#a);
        assert_eq!(quoted.to_string(), "hello");
        let t = format!("{quoted:#?}");
        expect![[r#"
            SUBTREE $$ 937550:Root[0000, 0]@0..0#ROOT2024 937550:Root[0000, 0]@0..0#ROOT2024
              IDENT   hello 937550:Root[0000, 0]@0..0#ROOT2024"#]]
        .assert_eq(&t);
    }

    #[test]
    fn test_quote_simple_derive_copy() {
        let name = mk_ident("Foo");

        let quoted = quote! {DUMMY =>
            impl Clone for #name {
                fn clone(&self) -> Self {
                    Self {}
                }
            }
        };

        assert_eq!(quoted.to_string(), "impl Clone for Foo {fn clone (& self) -> Self {Self {}}}");
    }

    #[test]
    fn test_quote_derive_copy_hack() {
        // Assume the given struct is:
        // struct Foo {
        //  name: String,
        //  id: u32,
        // }
        let struct_name = mk_ident("Foo");
        let fields = [mk_ident("name"), mk_ident("id")];
        let fields = fields.iter().map(|it| quote!(DUMMY =>#it: self.#it.clone(), ));

        let mut builder = tt::TopSubtreeBuilder::new(crate::tt::Delimiter {
            kind: crate::tt::DelimiterKind::Brace,
            open: DUMMY,
            close: DUMMY,
        });
        fields.for_each(|field| builder.extend_with_tt(field.view().as_token_trees()));
        let list = builder.build();

        let quoted = quote! {DUMMY =>
            impl Clone for #struct_name {
                fn clone(&self) -> Self {
                    Self #list
                }
            }
        };

        assert_eq!(
            quoted.to_string(),
            "impl Clone for Foo {fn clone (& self) -> Self {Self {name : self . name . clone () , id : self . id . clone () ,}}}"
        );
    }

    fn top(span: tt::Span) -> tt::TopSubtreeBuilder {
        tt::TopSubtreeBuilder::new(tt::Delimiter {
            kind: tt::DelimiterKind::Invisible,
            open: span,
            close: span,
        })
    }

    /// `quote_into!` into one builder, with `Group` and `Emit` splices, gives the very tree
    /// (bytes, spans, symbols) nested `quote!`s spliced into an outer one give.
    #[test]
    fn quote_into_matches_nested_quote() {
        let name = mk_ident("Foo");
        let fields = [mk_ident("a"), mk_ident("b"), mk_ident("c")];
        let empty = tt::TopSubtree::empty(tt::DelimSpan::from_single(OTHER));

        let nested = {
            let items = fields.iter().map(|it| quote!(OTHER => #it : self . #it , ));
            let body = quote!(DUMMY => { # #items });
            let path = quote!(OTHER => #name :: #name);
            let single = quote!(OTHER => #name);
            let nothing = quote!(DUMMY =>);
            quote!(DUMMY => impl #path for #single #nothing #empty #body)
        };

        let flat = {
            let mut b = top(DUMMY);
            let items = Emit(|b: &mut tt::TopSubtreeBuilder| {
                for it in &fields {
                    let item = Group(OTHER, |b: &mut tt::TopSubtreeBuilder| {
                        quote_into!(b, OTHER => #it : self . #it , )
                    });
                    quote_into!(b, OTHER => #item);
                }
            });
            let body = Emit(|b: &mut tt::TopSubtreeBuilder| quote_into!(b, DUMMY => { #items }));
            let path = Group(OTHER, |b: &mut tt::TopSubtreeBuilder| {
                quote_into!(b, OTHER => #name :: #name)
            });
            let single = Group(OTHER, |b: &mut tt::TopSubtreeBuilder| quote_into!(b, OTHER => #name));
            let nothing = Group(DUMMY, |_: &mut tt::TopSubtreeBuilder| {});
            quote_into!(&mut b, DUMMY => impl #path for #single #nothing #empty #body);
            b.build_skip_top_subtree()
        };

        assert_eq!(format!("{nested:#?}"), format!("{flat:#?}"));
        assert_eq!(nested.to_string(), flat.to_string());
        assert!(nested == flat);
    }

    /// A spliced `TopSubtree`, by value or by reference, and a `String` by reference, give
    /// the same tokens as before they were taken by reference.
    #[test]
    fn splices_by_reference() {
        let inner = quote!(DUMMY => a b);
        let by_ref = &inner;
        let text = String::from("x\"y\u{e9}\n");
        let text_ref = &text;
        let one = quote!(DUMMY => #inner #by_ref #text #text_ref);
        let mut b = top(DUMMY);
        b.extend_with_tt(inner.as_token_trees());
        b.extend_with_tt(inner.as_token_trees());
        let lit = tt::Literal {
            text_and_suffix: Symbol::intern(&text.escape_default().to_string()),
            span: DUMMY,
            kind: tt::LitKind::Str,
            suffix_len: 0,
        };
        b.push(lit.clone().into());
        b.push(lit.into());
        assert!(one == b.build_skip_top_subtree());
    }

    /// The cached symbol of a quoted identifier is the one `Symbol::intern` gives, declared or
    /// not, on every expansion.
    #[test]
    fn cached_ident_symbols() {
        for _ in 0..3 {
            let quoted = quote!(DUMMY => impl ra_expand_builder __self_discr Self);
            let symbols: Vec<_> = quoted
                .token_trees()
                .iter()
                .map(|it| match it {
                    tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => {
                        assert_eq!(ident.is_raw, IdentIsRaw::No);
                        ident.sym
                    }
                    _ => panic!("not an ident"),
                })
                .collect();
            let expected: Vec<_> = ["impl", "ra_expand_builder", "__self_discr", "Self"]
                .into_iter()
                .map(Symbol::intern)
                .collect();
            assert_eq!(symbols, expected);
            assert_eq!(symbols[0], intern::sym::kw_impl);
        }
    }

    /// The string literal fast path is `escape_default` exactly.
    #[test]
    fn str_literal_escapes_as_escape_default() {
        let mut cases: Vec<String> = [
            "", "plain text 123", "a\"b", "it's", "back\\slash", "tab\t", "nl\n", "café", "\u{0}",
            "~ !", "\"", "\n\n\nend", "start\u{7f}", "日本語", "mixed é \u{1F600} \\ \" ' \r\t x",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // Every character up to U+00FF, and a few beyond, alone and between kept runs.
        let chars: String = (0u32..0x100).chain([0x2028, 0xFEFF, 0x1F600]).filter_map(char::from_u32).collect();
        cases.push(chars.clone());
        cases.push(chars.chars().flat_map(|c| [c, 'k', 'e', 'p', 't']).collect());
        for s in &cases {
            let lit = super::str_literal(s, DUMMY);
            assert_eq!(lit.text_and_suffix.as_str(), s.escape_default().to_string(), "{s:?}");
            assert_eq!(lit.kind, tt::LitKind::Str);
        }
    }

    /// `Name` and `Symbol` become the identifier the old re-interning gave.
    #[test]
    fn name_and_symbol_idents() {
        fn reinterned(text: &str) -> tt::Ident {
            let (is_raw, s) = IdentIsRaw::split_from_symbol(text);
            tt::Ident { sym: Symbol::intern(s), span: DUMMY, is_raw }
        }
        for text in ["foo", "type", "r#type", "Self", "_x"] {
            let sym = Symbol::intern(text);
            let quoted = quote!(DUMMY => #sym);
            let Some(tt::TtElement::Leaf(tt::Leaf::Ident(ident))) = quoted.token_trees().iter().next()
            else {
                panic!("not an ident");
            };
            assert_eq!(ident, reinterned(text));
        }
        let name = Name::new_symbol_root(Symbol::intern("field"));
        let quoted = quote!(DUMMY => #name);
        let Some(tt::TtElement::Leaf(tt::Leaf::Ident(ident))) = quoted.token_trees().iter().next()
        else {
            panic!("not an ident");
        };
        assert_eq!(ident, reinterned("field"));
    }
}
