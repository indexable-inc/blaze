//! Builtin attributes.
use base_db::Db;
use blaze_mem::Frame;
use intern::sym;
use span::Span;

use crate::{ExpandResult, MacroCallId, MacroCallKind, name, tt};

use super::quote;

macro_rules! register_builtin {
    ($(($name:ident, $variant:ident) => $expand:ident),* $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinAttrExpander {
            $($variant),*
        }

        impl BuiltinAttrExpander {
            /// Every variant, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$variant),*];

            pub fn expander(&self) -> for<'f> fn (&Db, &'f Frame, MacroCallId, &tt::TopSubtree, Span) -> ExpandResult<tt::TopSubtree<&'f Frame>>  {
                match *self {
                    $( BuiltinAttrExpander::$variant => $expand, )*
                }
            }

            /// The builtin attribute `name` names: its declared index and one load.
            fn find_by_name(name: &name::Name) -> Option<Self> {
                static BY_SYMBOL: intern::KnownNames =
                    intern::KnownNames::new(&[$( sym::Declared::$name, )*]);
                BY_SYMBOL.get(name.symbol()).map(|at| Self::ALL[usize::from(at)])
            }

            /// Each builtin attribute's name, in declaration order.
            #[cfg(test)]
            const NAMES: &[intern::Symbol] = &[$( sym::$name, )*];

            /// [`Self::find_by_name`] as a chain of compares, the reference it is tested by.
            #[cfg(test)]
            fn find_by_name_by_compare(name: &name::Name) -> Option<Self> {
                match name {
                    $( id if id == &sym::$name => Some(BuiltinAttrExpander::$variant), )*
                     _ => None,
                }
            }
        }

    };
}

impl BuiltinAttrExpander {
    pub fn expand<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
        self.expander()(db, frame, id, tt, span)
    }

    pub fn is_derive(self) -> bool {
        matches!(self, BuiltinAttrExpander::Derive | BuiltinAttrExpander::DeriveConst)
    }
    pub fn is_test(self) -> bool {
        matches!(self, BuiltinAttrExpander::Test)
    }
    pub fn is_bench(self) -> bool {
        matches!(self, BuiltinAttrExpander::Bench)
    }
    pub fn is_test_case(self) -> bool {
        matches!(self, BuiltinAttrExpander::TestCase)
    }
}

register_builtin! {
    (bench, Bench) => dummy_gate_test_expand,
    (cfg_accessible, CfgAccessible) => dummy_attr_expand,
    (cfg_eval, CfgEval) => dummy_attr_expand,
    (derive, Derive) => derive_expand,
    // derive const is equivalent to derive for our proposes.
    (derive_const, DeriveConst) => derive_expand,
    (global_allocator, GlobalAllocator) => global_allocator_expand,
    (test, Test) => dummy_gate_test_expand,
    (test_case, TestCase) => dummy_gate_test_expand,
    (define_opaque, DefineOpaque) => define_opaque_expand,
}

pub fn find_builtin_attr(ident: &name::Name) -> Option<BuiltinAttrExpander> {
    BuiltinAttrExpander::find_by_name(ident)
}

fn dummy_attr_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    _span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    ExpandResult::ok(tt::TopSubtree::from_subtree_in(tt.view(), frame))
}

/// The static, and what calls it in the allocator's place: rustc's expansion defines `__rg_alloc`
/// and the others, which its allocator shim calls. These have names of blaze's, which the
/// stand-in rustc builds for a crate whose code is blaze's calls from a `#[global_allocator]`
/// of its own (blaze's `stub.rs`).
fn global_allocator_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let ident = |it: tt::TtElement<'_>| match it {
        tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => Some(ident.clone()), // clone: a copy (symbol and span); `#[global_allocator]`, once per crate
        _ => None,
    };
    let mut idents = tt.iter().filter_map(ident);
    idents.find(|it| it.sym.as_str() == "static");
    let Some(name) = idents.find(|it| it.sym.as_str() != "mut") else {
        return ExpandResult::ok(tt::TopSubtree::from_subtree_in(tt.view(), frame));
    };
    let name = &name;
    let result = quote::quote_in! { frame, span=>
        #tt
        #[export_name = "__blaze_global_alloc"]
        unsafe extern "C" fn __blaze_global_alloc(size: usize, align: usize) -> *mut u8 {
            unsafe {
                ::core::alloc::GlobalAlloc::alloc(
                    &#name,
                    ::core::alloc::Layout::from_size_align_unchecked(size, align),
                )
            }
        }
        #[export_name = "__blaze_global_dealloc"]
        unsafe extern "C" fn __blaze_global_dealloc(ptr: *mut u8, size: usize, align: usize) {
            unsafe {
                ::core::alloc::GlobalAlloc::dealloc(
                    &#name,
                    ptr,
                    ::core::alloc::Layout::from_size_align_unchecked(size, align),
                )
            }
        }
        #[export_name = "__blaze_global_realloc"]
        unsafe extern "C" fn __blaze_global_realloc(
            ptr: *mut u8,
            size: usize,
            align: usize,
            new_size: usize,
        ) -> *mut u8 {
            unsafe {
                ::core::alloc::GlobalAlloc::realloc(
                    &#name,
                    ptr,
                    ::core::alloc::Layout::from_size_align_unchecked(size, align),
                    new_size,
                )
            }
        }
        #[export_name = "__blaze_global_alloc_zeroed"]
        unsafe extern "C" fn __blaze_global_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
            unsafe {
                ::core::alloc::GlobalAlloc::alloc_zeroed(
                    &#name,
                    ::core::alloc::Layout::from_size_align_unchecked(size, align),
                )
            }
        }
    };
    ExpandResult::ok(result)
}

/// The item keeps the list as a tool attribute, which type inference reads and name resolution
/// does not expand again.
fn define_opaque_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let MacroCallKind::Attr { attr_args: Some(args), .. } = &id.loc(db).kind else {
        return ExpandResult::ok(tt::TopSubtree::from_subtree_in(tt.view(), frame));
    };
    let args = &**args;
    let result = quote::quote_in! { frame, span=>
        #[rust_analyzer::define_opaque(#args)]
        #tt
    };
    ExpandResult::ok(result)
}

fn dummy_gate_test_expand<'f>(
    _db: &Db,
    frame: &'f Frame,
    _id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let result = quote::quote_in! { frame, span=>
        #[cfg(test)]
        #tt
    };
    ExpandResult::ok(result)
}

/// We generate a very specific expansion here, as we do not actually expand the `#[derive]` attribute
/// itself in name res, but we do want to expand it to something for the IDE layer, so that the input
/// derive attributes can be downmapped, and resolved as proper paths.
/// This is basically a hack, that simplifies the hacks we need in a lot of ide layer places to
/// somewhat inconsistently resolve derive attributes.
///
/// As such, we expand `#[derive(Foo, bar::Bar)]` into
/// ```ignore
///  #![Foo]
///  #![bar::Bar]
/// ```
/// which allows fallback path resolution in hir::view::Semantics to properly identify our derives.
/// Since we do not expand the attribute in nameres though, we keep the original item.
///
/// The ideal expansion here would be for the `#[derive]` to re-emit the annotated item and somehow
/// use the input paths in its output as well.
/// But that would bring two problems with it, for one every derive would duplicate the item token tree
/// wasting a lot of memory, and it would also require some way to use a path in a way that makes it
/// always resolve as a derive without nameres recollecting them.
/// So this hacky approach is a lot more friendly for us, though it does require a bit of support in
/// hir::view::Semantics to make this work.
fn derive_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let loc = id.loc(db);
    let derives = match &loc.kind {
        MacroCallKind::Attr { attr_args: Some(attr_args), .. } if loc.def.is_attribute_derive() => {
            attr_args
        }
        _ => {
            return ExpandResult::ok(tt::TopSubtree::empty_in(
                tt::DelimSpan { open: span, close: span },
                frame,
            ));
        }
    };
    pseudo_derive_attr_expansion(frame, tt, derives, span)
}

pub fn pseudo_derive_attr_expansion<'f>(
    frame: &'f Frame,
    _: &tt::TopSubtree,
    args: &tt::TopSubtree,
    call_site: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let mk_leaf =
        |char| tt::Leaf::Punct(tt::Punct { char, spacing: tt::Spacing::Alone, span: call_site });

    // Each of the `n` paths gains `#`, `!` and a bracket, and loses its comma: at most
    // `len + 2n + 1` token trees, and `n <= len + 1`.
    let len = args.token_trees().len();
    let mut token_trees =
        tt::TopSubtreeBuilder::with_capacity(args.top_subtree().delimiter, 3 * len + 3);
    let iter = args.token_trees().split(|tt| {
        matches!(tt, tt::TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ',', .. })))
    });
    for tts in iter {
        token_trees.extend([mk_leaf('#'), mk_leaf('!')]);
        token_trees.open(tt::DelimiterKind::Bracket, call_site);
        token_trees.extend_with_tt(tts);
        token_trees.close(call_site);
    }
    ExpandResult::ok(token_trees.build_in(frame))
}

#[cfg(test)]
mod tests {
    use intern::{Symbol, sym};

    use super::BuiltinAttrExpander;
    use crate::name::Name;

    #[test]
    fn builtin_attrs_found_as_by_compare() {
        let mut symbols: Vec<Symbol> = BuiltinAttrExpander::NAMES.to_vec();
        let texts: Vec<String> = symbols.iter().map(|it| it.as_str().to_owned()).collect();
        for text in &texts {
            symbols.push(Symbol::intern(text));
            symbols.push(Symbol::intern(&format!("{text}_")));
            symbols.push(Symbol::intern(&text.to_uppercase()));
            symbols.push(Symbol::intern(&text[..text.len() - 1]));
        }
        symbols.extend([sym::self_, sym::cfg, Symbol::intern("inline"), Symbol::intern("tokio_test")]);
        for symbol in symbols {
            let name = Name::new_symbol_root(symbol);
            assert_eq!(
                BuiltinAttrExpander::find_by_name(&name),
                BuiltinAttrExpander::find_by_name_by_compare(&name),
                "{name:?}"
            );
        }
        for (at, symbol) in BuiltinAttrExpander::NAMES.iter().enumerate() {
            let name = Name::new_symbol_root(symbol.clone());
            assert_eq!(BuiltinAttrExpander::find_by_name(&name), Some(BuiltinAttrExpander::ALL[at]));
        }
    }
}
