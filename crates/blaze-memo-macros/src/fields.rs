//! The fields of an input or interned struct and their attributes: `#[returns(..)]` and
//! `#[get(name)]`. blaze-query's `#[set]`, `#[default]`, `#[tracked]`, `#[no_eq]` and
//! `#[value(..)]` are refused with what replaces them; any other attribute (docs) goes on the
//! field's getter.

use syn::ext::IdentExt;
use syn::{Attribute, Ident, Type, Visibility};

use crate::returns::Mode;

/// Which struct the fields are of.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Input,
    Interned,
}

impl Kind {
    fn attribute(self) -> &'static str {
        match self {
            Kind::Input => "`#[blaze_memo::input]`",
            Kind::Interned => "`#[blaze_memo::interned]`",
        }
    }
}

pub(crate) struct Field {
    pub(crate) ident: Ident,
    pub(crate) vis: Visibility,
    pub(crate) ty: Type,
    pub(crate) mode: Mode,
    pub(crate) getter: Ident,
    pub(crate) attrs: Vec<Attribute>,
}

const BANNED: &[&str] = &["new", "set", "get", "try_get", "gid", "from_gid"];

/// blaze-query's field attributes with no meaning here, and why.
const REMOVED: &[(&str, &str)] = &[
    (
        "set",
        "a snapshot's inputs never change: the driver makes the next snapshot with the new \
         value (`Memo::next`, then `new` in the arena it dropped)",
    ),
    ("default", "every field is given when the input is made"),
    ("tracked", "tracked structs are gone: make it an interned struct"),
    ("no_eq", "tracked structs are gone: make it an interned struct"),
    (
        "value",
        "a field owns no memory to drop, checked at compile time (an input's fields may), so \
         there is no retention to prove",
    ),
];

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
        // alloc: expansion time, once per annotated item; no query runs this
        attrs: Vec::new(),
    };
    let mut returns_seen = false;
    let mut get_seen = false;
    for attr in &field.attrs {
        let Some(word) = attr.path().get_ident() else {
            out.attrs.push(attr.clone()); // clone: expansion time, an attribute
            continue;
        };
        // alloc: expansion time, once per attribute
        let text = word.to_string();
        if let Some((_, why)) = REMOVED.iter().find(|(it, _)| *it == text) {
            return Err(syn::Error::new_spanned(
                attr,
                // alloc: expansion time, the error's message
                format!("`#[{text}]` is not a field attribute of {}: {why}", kind.attribute()),
            ));
        }
        let seen = match text.as_str() {
            "returns" => &mut returns_seen,
            "get" => &mut get_seen,
            _ => {
                out.attrs.push(attr.clone()); // clone: expansion time, an attribute
                continue;
            }
        };
        if *seen {
            return Err(syn::Error::new_spanned(
                attr,
                // alloc: expansion time, the error's message
                format!("`#[{text}]` is given twice"),
            ));
        }
        *seen = true;
        if text == "returns" {
            let mode = attr.parse_args_with(Ident::parse_any)?;
            out.mode = Mode::parse(&mode)?;
        } else {
            out.getter = attr.parse_args()?;
        }
    }
    Ok(out)
}
