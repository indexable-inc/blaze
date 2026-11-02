//! The fields of an input, interned or tracked struct and their attributes: `#[returns(..)]`,
//! `#[get(name)]`, `#[set(name)]`, `#[default]`, `#[tracked]`, `#[no_eq]` and
//! `#[value(unsafe(..))]`. Any other attribute (docs) goes on the field's getter.

use syn::ext::IdentExt;
use syn::{Attribute, Ident, Type, Visibility};

use crate::returns::Mode;

/// Which struct the fields are of: what each may carry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Input,
    Interned,
    Tracked,
}

impl Kind {
    fn attribute(self) -> &'static str {
        match self {
            Kind::Input => "`#[blaze_query::input]`",
            Kind::Interned => "`#[blaze_query::interned]`",
            Kind::Tracked => "`#[blaze_query::tracked]`",
        }
    }
}

pub(crate) struct Field {
    pub(crate) ident: Ident,
    pub(crate) vis: Visibility,
    pub(crate) ty: Type,
    pub(crate) mode: Mode,
    pub(crate) getter: Ident,
    pub(crate) setter: Ident,
    pub(crate) default: bool,
    pub(crate) tracked: bool,
    pub(crate) no_eq: bool,
    /// `#[value(unsafe(..))]`: the user's proof that the field may be stored with the database
    /// lifetime erased.
    pub(crate) proven: Option<Attribute>,
    pub(crate) attrs: Vec<Attribute>,
}

const BANNED: &[&str] = &["from", "new"];

pub(crate) fn parse(kind: Kind, item: &syn::ItemStruct) -> syn::Result<Vec<Field>> {
    let syn::Fields::Named(named) = &item.fields else {
        return Err(syn::Error::new_spanned(
            &item.ident,
            // alloc: expansion time, the error's message
            format!("{} goes on a struct with named fields", kind.attribute()),
        ));
    };
    // alloc: expansion time, once per annotated item; no query runs this
    named.named.iter().map(|field| parse_field(kind, field)).collect()
}

fn parse_field(kind: Kind, field: &syn::Field) -> syn::Result<Field> {
    let ident = field.ident.as_ref().expect("a named field has a name");
    // alloc: expansion time, once per field
    let name = ident.unraw().to_string();
    if BANNED.contains(&name.as_str()) {
        return Err(syn::Error::new(
            ident.span(),
            // alloc: expansion time, the error's message
            format!("a field can't be named `{name}`: the struct has a function of that name"),
        ));
    }
    let mut out = Field {
        ident: ident.clone(),   // clone: expansion time, an identifier
        vis: field.vis.clone(), // clone: expansion time, a visibility
        ty: field.ty.clone(),   // clone: expansion time, a type
        mode: Mode::Ref,
        getter: ident.clone(), // clone: expansion time, an identifier
        setter: syn::Ident::new(&format!("set_{name}"), ident.span()),
        default: false,
        tracked: false,
        no_eq: false,
        proven: None,
        // alloc: expansion time, once per annotated item; no query runs this
        attrs: Vec::new(),
    };
    // alloc: expansion time, once per annotated item; no query runs this
    let mut seen: Vec<&str> = Vec::new();
    for attr in &field.attrs {
        let Some(word) = attr.path().get_ident() else {
            out.attrs.push(attr.clone()); // clone: expansion time, an attribute
            continue;
        };
        // alloc: expansion time, once per annotated item; no query runs this
        let allowed: &[Kind] = match word.to_string().as_str() {
            "returns" => &[Kind::Input, Kind::Interned, Kind::Tracked],
            "get" => &[Kind::Input, Kind::Interned, Kind::Tracked],
            "set" | "default" => &[Kind::Input],
            "tracked" | "no_eq" => &[Kind::Tracked],
            "value" => &[Kind::Interned],
            _ => {
                out.attrs.push(attr.clone()); // clone: expansion time, an attribute
                continue;
            }
        };
        // alloc: expansion time, once per annotated item; no query runs this
        let word_name = match word.to_string().as_str() {
            "returns" => "returns",
            "get" => "get",
            "set" => "set",
            "default" => "default",
            "tracked" => "tracked",
            "no_eq" => "no_eq",
            _ => "value",
        };
        if !allowed.contains(&kind) {
            return Err(syn::Error::new_spanned(
                attr,
                // alloc: expansion time, the error's message
                format!("`#[{word_name}]` is not a field attribute of {}", kind.attribute()),
            ));
        }
        if seen.contains(&word_name) {
            return Err(syn::Error::new_spanned(
                attr,
                // alloc: expansion time, the error's message
                format!("`#[{word_name}]` is given twice"),
            ));
        }
        seen.push(word_name);
        match word_name {
            "returns" => {
                let mode = attr.parse_args_with(Ident::parse_any)?;
                out.mode = Mode::parse(&mode)?;
            }
            "get" => out.getter = attr.parse_args()?,
            "set" => out.setter = attr.parse_args()?,
            "default" => {
                attr.meta.require_path_only()?;
                out.default = true;
            }
            "tracked" => {
                attr.meta.require_path_only()?;
                out.tracked = true;
            }
            "no_eq" => {
                attr.meta.require_path_only()?;
                out.no_eq = true;
            }
            _ => out.proven = Some(attr.clone()), // clone: expansion time, an attribute
        }
    }
    if out.no_eq && !out.tracked {
        return Err(syn::Error::new_spanned(&field.ty, "`#[no_eq]` goes on a `#[tracked]` field"));
    }
    Ok(out)
}
