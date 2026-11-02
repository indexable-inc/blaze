//! Builtin attributes.
use base_db::Db;
use blaze_mem::Frame;
use intern::sym;
use span::Span;

use crate::{ExpandError, ExpandResult, MacroCallId, MacroCallKind, name, tt};

use super::quote;

mod contract_test;
use contract_test::contract_test_expand;

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
    // Nightly's contracts (`core::contracts::{requires, ensures}`). Appended: the nameres image
    // stores a builtin attribute by its index in `ALL`.
    (contracts_requires, ContractsRequires) => contracts_requires_expand,
    (contracts_ensures, ContractsEnsures) => contracts_ensures_expand,
    // `#[blaze::contract_test]` (`core::blaze::contract_test`), appended as above.
    (contract_test, ContractTest) => contract_test_expand,
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

/// `#[core::contracts::requires(DECLS; COND)]`, as rustc's `rustc_builtin_macros::contracts`.
fn contracts_requires_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    contract_clause_expand(db, frame, id, tt, span, ContractClause::Requires)
}

/// `#[core::contracts::ensures(|ret| COND)]`, as rustc's `rustc_builtin_macros::contracts`.
fn contracts_ensures_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    contract_clause_expand(db, frame, id, tt, span, ContractClause::Ensures)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContractClause {
    Requires,
    Ensures,
}

impl ContractClause {
    /// The attribute the clause leaves on its marker statement in the body.
    fn marker(self) -> intern::Symbol {
        match self {
            ContractClause::Requires => sym::rustc_contract_requires,
            ContractClause::Ensures => sym::rustc_contract_ensures,
        }
    }
}

/// The clause of a contract attribute, kept in the function's body as the marker statement
/// `#[rustc_contract_requires] { ARGS };` (or `rustc_contract_ensures`), after the body's inner
/// attributes and the clauses already there; hir-def's body lowering desugars it as rustc's
/// `rustc_ast_lowering::contract` does.
///
/// rustc injects `contract_requires { ARGS }` into the signature, which its parser takes at most
/// once and before at most one `contract_ensures`: a second clause of either kind, or a requires
/// expanded after an ensures (an ensures written above it), is the parse error it is there,
/// refused here. What rustc's expander refuses is refused here with its message; on an error the
/// item is kept as it was, which then fails to build.
fn contract_clause_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
    clause: ContractClause,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let unchanged = || tt::TopSubtree::from_subtree_in(tt.view(), frame);
    let fail = |msg: &str| ExpandResult::new(unchanged(), ExpandError::other(span, msg));
    let args = match &id.loc(db).kind {
        MacroCallKind::Attr { attr_args: Some(args), .. } if !args.token_trees().is_empty() => &**args,
        _ => {
            let (name, example) = match clause {
                ContractClause::Requires => ("requires", "condition"),
                ContractClause::Ensures => ("ensures", "|result: &T| condition"),
            };
            // alloc: an error's message, once per refused attribute
            return fail(&format!(
                "`{name}` attribute requires an argument, e.g., `#[{name}({example})]`"
            ));
        }
    };
    let is_kw = |it: &tt::TtElement<'_>, kw: &str| {
        matches!(it, tt::TtElement::Leaf(tt::Leaf::Ident(ident)) if ident.is_raw == tt::IdentIsRaw::No && ident.sym.as_str() == kw)
    };
    let elements = tt.iter();
    let mut before_fn = elements.clone();
    let Some(fn_at) = before_fn.position(|it| is_kw(&it, "fn")) else {
        return fail("contract annotations can only be used on functions");
    };
    if elements.clone().take(fn_at).any(|it| is_kw(&it, "async") || is_kw(&it, "gen")) {
        return fail("contract annotations are not yet supported on async or gen functions");
    }
    // The body is the last token tree, a brace group, and there is one after the `fn`.
    let count = elements.clone().count();
    let body = match elements.clone().last() {
        Some(tt::TtElement::Subtree(subtree, body))
            if count > fn_at + 1 && subtree.delimiter.kind == tt::DelimiterKind::Brace =>
        {
            (subtree, body)
        }
        _ => return fail("contract annotations is only supported in functions with bodies"),
    };
    let (body_subtree, body) = body;

    let punct = |char| tt::Leaf::Punct(tt::Punct { char, spacing: tt::Spacing::Alone, span });
    let is_punct = |it: &Option<tt::TtElement<'_>>, ch: char| {
        matches!(it, Some(tt::TtElement::Leaf(tt::Leaf::Punct(p))) if p.char == ch)
    };
    let mut rest = body.clone();
    // The body's inner attributes (`#![..]`, inner doc comments) stay first.
    let mut kept = 0;
    loop {
        let mut probe = rest.clone();
        let (hash, bang, group) = (probe.next(), probe.next(), probe.next());
        let is_inner = is_punct(&hash, '#')
            && is_punct(&bang, '!')
            && matches!(group, Some(tt::TtElement::Subtree(it, _)) if it.delimiter.kind == tt::DelimiterKind::Bracket);
        if !is_inner {
            break;
        }
        rest = probe;
        kept += 3;
    }
    // The clauses expanded before this one: `#[rustc_contract_*] { .. };`.
    loop {
        let mut probe = rest.clone();
        let hash = probe.next();
        let marker = match probe.next() {
            Some(tt::TtElement::Subtree(it, mut inner))
                if is_punct(&hash, '#') && it.delimiter.kind == tt::DelimiterKind::Bracket =>
            {
                match (inner.next(), inner.next()) {
                    (Some(tt::TtElement::Leaf(tt::Leaf::Ident(ident))), None)
                        if ident.sym == sym::rustc_contract_requires
                            || ident.sym == sym::rustc_contract_ensures =>
                    {
                        ident.sym
                    }
                    _ => break,
                }
            }
            _ => break,
        };
        // A requires after any clause, or a second ensures, is what rustc's parser refuses.
        if clause == ContractClause::Requires || marker == sym::rustc_contract_ensures {
            let (name, earlier) = match (clause, marker == sym::rustc_contract_ensures) {
                (ContractClause::Requires, true) => ("requires", "an `ensures` clause above it"),
                (ContractClause::Requires, false) => ("requires", "a second `requires`"),
                (ContractClause::Ensures, _) => ("ensures", "a second `ensures`"),
            };
            // alloc: an error's message, once per refused attribute
            return fail(&format!(
                "contract `{name}` attribute refused: a function takes at most one `requires` and then \
                 at most one `ensures`, written in that order, and this one has {earlier}"
            ));
        }
        let (block, semi) = (probe.next(), probe.next());
        if !matches!(block, Some(tt::TtElement::Subtree(it, _)) if it.delimiter.kind == tt::DelimiterKind::Brace)
            || !is_punct(&semi, ';')
        {
            break;
        }
        rest = probe;
        kept += 4;
    }

    let mut out = tt::TopSubtreeBuilder::with_capacity(
        tt.top_subtree().delimiter,
        tt.token_trees().len() + args.token_trees().len() + 8,
    );
    let mut item = elements;
    for _ in 0..count - 1 {
        if let Some(view) = item.next_as_view() {
            out.extend_with_tt(view);
        }
    }
    out.open(tt::DelimiterKind::Brace, body_subtree.delimiter.open);
    let mut body_iter = body.clone();
    for _ in 0..kept {
        if let Some(view) = body_iter.next_as_view() {
            out.extend_with_tt(view);
        }
    }
    out.push(punct('#'));
    out.open(tt::DelimiterKind::Bracket, span);
    out.push(tt::Leaf::Ident(tt::Ident { sym: clause.marker(), span, is_raw: tt::IdentIsRaw::No }));
    out.close(span);
    let args_delim = args.top_subtree().delimiter;
    out.open(tt::DelimiterKind::Brace, args_delim.open);
    out.extend_with_tt(args.token_trees());
    out.close(args_delim.close);
    out.push(punct(';'));
    out.extend_with_tt(rest.remaining());
    out.close(body_subtree.delimiter.close);
    ExpandResult::ok(out.build_in(frame))
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
