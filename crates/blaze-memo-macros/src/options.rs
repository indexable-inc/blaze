//! The options of each attribute (`(returns(ref), cycle_result = f, ..)`, commas between, a
//! trailing one allowed). An option the site doesn't take is an error naming it; the options
//! of blaze-query the memo has no meaning for are refused with what replaces them.

use proc_macro2::TokenStream;
use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::{Expr, Ident, Token, Type, parenthesized};

use crate::returns::Mode;

/// Which attribute the options are for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Site {
    TrackedFn,
    Input,
    Interned,
}

impl Site {
    fn attribute(self) -> &'static str {
        match self {
            Site::TrackedFn => "`#[blaze_memo::tracked]` on a function",
            Site::Input => "`#[blaze_memo::input]`",
            Site::Interned => "`#[blaze_memo::interned]`",
        }
    }
}

#[derive(Default)]
pub(crate) struct Options {
    pub(crate) returns: Option<(Ident, Mode)>,
    pub(crate) debug: Option<Ident>,
    pub(crate) no_lifetime: Option<Ident>,
    pub(crate) singleton: Option<Ident>,
    pub(crate) cycle_fn: Option<Expr>,
    pub(crate) cycle_initial: Option<Expr>,
    pub(crate) cycle_result: Option<Expr>,
    pub(crate) constructor: Option<Ident>,
    pub(crate) self_ty: Option<Type>,
}

/// blaze-query's options with no meaning here, and why.
const REMOVED: &[(&str, &str)] = &[
    (
        "lru",
        "the memo evicts nothing by hand: an arena is dropped whole when its crate changes, and \
         `Config::cap_bytes` bounds what a snapshot carries",
    ),
    ("weigh", "the memo weighs no value: an arena's bytes are its weight (`Memo::held`)"),
    (
        "no_eq",
        "the memo compares no values outside a fixpoint (there is no backdating); a \
         `cycle_initial` query's values are compared by `PartialEq`",
    ),
    (
        "scope",
        "a value lives in its key's arena for the snapshot; a task-scoped table is gone (keep \
         a per-task result in the task, outside the memo)",
    ),
    ("settled", "a value is final when its run ends: nothing is told it settled"),
    (
        "mem_tag",
        "an entry is counted to its arena's tag, set by the driver (`Builder::arena(index, tag)`)",
    ),
];

impl Options {
    pub(crate) fn parse(site: Site, tokens: TokenStream) -> syn::Result<Options> {
        (|input: ParseStream<'_>| Options::parse_in(site, input)).parse2(tokens)
    }

    fn parse_in(site: Site, input: ParseStream<'_>) -> syn::Result<Options> {
        let mut options = Options::default();
        while !input.is_empty() {
            let name = Ident::parse_any(input)?;
            let allowed = |sites: &[Site]| -> syn::Result<()> {
                if sites.contains(&site) {
                    Ok(())
                } else {
                    Err(syn::Error::new(
                        name.span(),
                        // alloc: expansion time, the error's message
                        format!("`{name}` is not an option of {}", site.attribute()),
                    ))
                }
            };
            // alloc: expansion time, once per option
            let text = name.to_string();
            if let Some((_, why)) = REMOVED.iter().find(|(it, _)| *it == text) {
                return Err(syn::Error::new(
                    name.span(),
                    // alloc: expansion time, the error's message
                    format!("`{name}` is not an option of blaze-memo: {why}"),
                ));
            }
            match text.as_str() {
                "returns" => {
                    allowed(&[Site::TrackedFn])?;
                    let content;
                    parenthesized!(content in input);
                    let mode_name = Ident::parse_any(&content)?;
                    let mode = Mode::parse(&mode_name)?;
                    set(&mut options.returns, &name, (mode_name, mode))?;
                }
                "debug" => {
                    allowed(&[Site::Input, Site::Interned])?;
                    set(&mut options.debug, &name, name.clone())?; // clone: expansion time, an identifier
                }
                "singleton" => {
                    allowed(&[Site::Input])?;
                    set(&mut options.singleton, &name, name.clone())?; // clone: expansion time, an identifier
                }
                "no_lifetime" => {
                    return Err(syn::Error::new(
                        name.span(),
                        "`no_lifetime` is written `unsafe(no_lifetime)`",
                    ));
                }
                "unsafe" => {
                    let content;
                    parenthesized!(content in input);
                    let inner = Ident::parse_any(&content)?;
                    if inner != "no_lifetime" || site != Site::Interned {
                        return Err(syn::Error::new(
                            inner.span(),
                            // alloc: expansion time, the error's message
                            format!("`unsafe({inner})` is not an option of {}", site.attribute()),
                        ));
                    }
                    set(&mut options.no_lifetime, &inner, inner.clone())?; // clone: expansion time, an identifier
                }
                "cycle_fn" => {
                    allowed(&[Site::TrackedFn])?;
                    input.parse::<Token![=]>()?;
                    set(&mut options.cycle_fn, &name, input.parse::<Expr>()?)?;
                }
                "cycle_initial" => {
                    allowed(&[Site::TrackedFn])?;
                    input.parse::<Token![=]>()?;
                    set(&mut options.cycle_initial, &name, input.parse::<Expr>()?)?;
                }
                "cycle_result" => {
                    allowed(&[Site::TrackedFn])?;
                    input.parse::<Token![=]>()?;
                    set(&mut options.cycle_result, &name, input.parse::<Expr>()?)?;
                }
                "constructor" => {
                    allowed(&[Site::Interned])?;
                    input.parse::<Token![=]>()?;
                    set(&mut options.constructor, &name, input.parse::<Ident>()?)?;
                }
                "self_ty" => {
                    allowed(&[Site::TrackedFn])?;
                    input.parse::<Token![=]>()?;
                    set(&mut options.self_ty, &name, input.parse::<Type>()?)?;
                }
                _ => {
                    return Err(syn::Error::new(
                        name.span(),
                        // alloc: expansion time, the error's message
                        format!("unknown option `{name}` of {}", site.attribute()),
                    ));
                }
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        options.check(site)?;
        Ok(options)
    }

    /// The combinations refused.
    fn check(&self, site: Site) -> syn::Result<()> {
        if site != Site::TrackedFn {
            return Ok(());
        }
        if let (Some(cycle_fn), None) = (&self.cycle_fn, &self.cycle_initial) {
            return Err(syn::Error::new_spanned(
                cycle_fn,
                "`cycle_fn` needs `cycle_initial`: the value a cycle starts from",
            ));
        }
        if let (Some(result), true) =
            (&self.cycle_result, self.cycle_initial.is_some() || self.cycle_fn.is_some())
        {
            return Err(syn::Error::new_spanned(
                result,
                "`cycle_result` is one way out of a cycle and `cycle_initial` with `cycle_fn` the \
                 other: give one",
            ));
        }
        Ok(())
    }
}

fn set<T>(slot: &mut Option<T>, name: &Ident, value: T) -> syn::Result<()> {
    if slot.is_some() {
        return Err(syn::Error::new(
            name.span(),
            // alloc: expansion time, the error's message
            format!("option `{name}` is given twice"),
        ));
    }
    *slot = Some(value);
    Ok(())
}
