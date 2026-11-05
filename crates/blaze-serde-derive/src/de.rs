//! `impl Deserialize<'de>`: a visitor with `visit_seq` and `visit_map` for a struct and for each
//! struct or tuple variant, `EnumAccess` for an externally tagged enum, and the buffered
//! `Content` of `blaze_serde::__private` for an internally tagged or untagged one.

use proc_macro::{Ident, Span, TokenStream};

use crate::{
    generics,
    out::Out,
    parse::{CowOf, Data, DefaultOf, Error, Field, Fields, Item, Style, Variant, at, unraw},
};

const DE: &str = "::blaze_serde::serde::de";
const OK: &str = "::core::result::Result::Ok";
const SOME: &str = "::core::option::Option::Some";
const NONE: &str = "::core::option::Option::None";
const MARKERS: &str =
    "{ __marker: ::core::marker::PhantomData, __lifetime: ::core::marker::PhantomData }";

pub(crate) fn derive(item: &Item) -> Result<TokenStream, Error> {
    check(item)?;
    let mut lead = String::from("'de");
    let borrowed = borrowed(item);
    if !borrowed.is_empty() {
        lead.push_str(": ");
        lead.push_str(&borrowed.join(" + "));
    }
    let mut cx = Gen { item, lead, bounds: bounds(item), wrappers: Vec::new(), visitors: 0 };
    let code = cx.body();
    let mut out = Out::default();
    out.s("#[automatically_derived] impl");
    item.generics.write_impl(&mut out, &[&cx.lead]);
    out.s("::blaze_serde::Deserialize<'de> for").ident(&item.ident);
    item.generics.write_args(&mut out);
    item.generics.write_where(&mut out, &cx.bounds);
    out.brace(|out| {
        out.s("fn deserialize<__D>(__deserializer: __D) -> ::core::result::Result<Self, __D::Error> where __D: ::blaze_serde::serde::Deserializer<'de>");
        out.brace(|out| {
            cx.write_wrappers(out);
            out.tokens(&code);
        });
    });
    Ok(out.finish())
}

/// What a `Deserialize` refuses that a `Serialize` takes.
fn check(item: &Item) -> Result<(), Error> {
    let refuse = |what: &str| -> Result<(), Error> {
        let span = item.ident.span();
        Err(Error::new(span, format!("`{}` ({}): {what}", item.name(), at(span))))
    };
    let newtype_skip = |fields: &Fields| {
        fields.style == Style::Tuple && fields.list.len() == 1 && fields.list[0].attrs.skip
    };
    match &item.data {
        Data::Struct(fields) => {
            if newtype_skip(fields) && !item.attrs.transparent {
                return refuse("`#[serde(skip)]` on the one field of a newtype struct");
            }
        }
        Data::Enum(variants) => {
            if variants.is_empty() {
                return refuse(
                    "blaze-serde-derive derives `Deserialize` for an enum with variants only",
                );
            }
            if item.attrs.tag.is_none() && variants.iter().any(|it| it.attrs.other) {
                return refuse("`#[serde(other)]` is read in an internally tagged enum only");
            }
            if variants.iter().any(|it| newtype_skip(&it.fields)) {
                return refuse("`#[serde(skip)]` on the one field of a newtype variant");
            }
        }
    }
    Ok(())
}

/// The lifetimes the fields borrow from the input, which `'de` outlives.
fn borrowed(item: &Item) -> Vec<String> {
    if item.attrs.from.is_some() || item.attrs.try_from.is_some() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for field in all_fields(item) {
        if field.attrs.skip {
            continue;
        }
        for lifetime in &field.attrs.borrowed {
            if !out.contains(lifetime) {
                out.push(lifetime.clone());
            }
        }
    }
    out
}

fn all_fields(item: &Item) -> Vec<&Field> {
    match &item.data {
        Data::Struct(fields) => fields.list.iter().collect(),
        Data::Enum(variants) => variants.iter().flat_map(|it| &it.fields.list).collect(),
    }
}

/// `T: Deserialize<'de>` for each type parameter a deserialized field names, `T: Default` for
/// each a defaulted one names; or `bound(deserialize)`; or, through `from`/`try_from`, that
/// type's `Deserialize` when it names a parameter.
fn bounds(item: &Item) -> Vec<TokenStream> {
    if let Some(bound) = &item.attrs.bound_deserialize {
        return bound.clone();
    }
    let predicate = |ty: &TokenStream, bound: &str| {
        let mut out = Out::default();
        out.tokens(ty).s(bound);
        out.finish()
    };
    if let Some(from) = item.attrs.from.as_ref().or(item.attrs.try_from.as_ref()) {
        let generic = item.generics.type_params().any(|it| generics::mentions(from, it));
        return match generic {
            true => vec![predicate(from, ": ::blaze_serde::Deserialize<'de>")],
            false => Vec::new(),
        };
    }
    let fields = all_fields(item);
    let mut out = Vec::new();
    for param in item.generics.type_params() {
        let ty: TokenStream = TokenStream::from(proc_macro::TokenTree::Ident(param.clone()));
        let named = |it: &&&Field| generics::mentions(&it.ty, param);
        if fields.iter().filter(named).any(|it| {
            !it.attrs.skip && it.attrs.deserialize_with.is_none() && it.attrs.borrow_cow.is_none()
        }) {
            out.push(predicate(&ty, ": ::blaze_serde::Deserialize<'de>"));
        }
        if fields.iter().filter(named).any(|it| {
            matches!(it.attrs.default, Some(DefaultOf::Trait))
                || (it.attrs.skip && it.attrs.default.is_none())
        }) {
            out.push(predicate(&ty, ": ::core::default::Default"));
        }
    }
    out
}

/// How a field is read: its own type, or a wrapper whose `value` it is.
struct Store {
    ty: TokenStream,
    wrapped: bool,
}

/// Where a visitor builds its value: the struct, or one variant of the enum.
struct Ctor<'i> {
    variant: Option<&'i Variant>,
    fields: &'i Fields,
    /// What the visitor's `expecting` says: `struct Item`, `tuple variant Item::V`.
    expecting: String,
}

struct Gen<'i> {
    item: &'i Item,
    /// `'de`, with the lifetimes borrowed: `'de: 'a`.
    lead: String,
    bounds: Vec<TokenStream>,
    /// The `deserialize_with` wrappers: field type and path.
    wrappers: Vec<(TokenStream, TokenStream)>,
    visitors: usize,
}

impl<'i> Gen<'i> {
    fn body(&mut self) -> TokenStream {
        let item = self.item;
        if let Some(from) = item.attrs.from.as_ref() {
            return Self::convert(from, false);
        }
        if let Some(from) = item.attrs.try_from.as_ref() {
            return Self::convert(from, true);
        }
        match (&item.data, &item.attrs.tag) {
            (Data::Struct(fields), _) if item.attrs.transparent => self.transparent(fields),
            (Data::Struct(fields), _) => self.top_struct(fields),
            (Data::Enum(variants), Some(tag)) => self.internally_tagged(variants, tag),
            (Data::Enum(variants), None) if item.attrs.untagged => self.untagged(variants),
            (Data::Enum(variants), None) => self.externally_tagged(variants),
        }
    }

    /// Through `from` or `try_from`: the other type read, then converted.
    fn convert(from: &TokenStream, fallible: bool) -> TokenStream {
        let mut out = Out::default();
        out.s("let __value = <")
            .tokens(from)
            .s("as ::blaze_serde::Deserialize<'de>>::deserialize(__deserializer)?;");
        match fallible {
            false => out
                .s(&format!("{OK}(<Self as ::core::convert::From<"))
                .tokens(from)
                .s(">>::from(__value))"),
            true => out
                .s("<Self as ::core::convert::TryFrom<")
                .tokens(from)
                .s(&format!(">>::try_from(__value).map_err(<__D::Error as {DE}::Error>::custom)")),
        };
        out.finish()
    }

    fn transparent(&mut self, fields: &Fields) -> TokenStream {
        let stores = self.stores(fields);
        let mut out = Out::default();
        for (field, store) in fields.list.iter().zip(&stores) {
            out.s(&format!("let __f{} =", field.index));
            match field.attrs.skip {
                true => {
                    default_of(&mut out, field, None);
                }
                false => {
                    out.s("<")
                        .tokens(&store.ty)
                        .s("as ::blaze_serde::Deserialize<'de>>::deserialize(__deserializer)?");
                    if store.wrapped {
                        out.s(".value");
                    }
                }
            }
            out.s(";");
        }
        out.s(OK).paren(|out| {
            construct(out, self.item, &Ctor { variant: None, fields, expecting: String::new() })
        });
        out.finish()
    }

    fn top_struct(&mut self, fields: &'i Fields) -> TokenStream {
        let item = self.item;
        let name = item.name();
        let expecting = match fields.style {
            Style::Named => format!("struct {name}"),
            Style::Tuple => format!("tuple struct {name}"),
            Style::Unit => format!("unit struct {name}"),
        };
        let ctor = Ctor { variant: None, fields, expecting };
        let deny = item.attrs.deny_unknown_fields;
        let (visitor, number) = self.visitor(&ctor, deny, item.attrs.default.as_ref());
        let mut out = Out::default();
        out.tokens(&visitor);
        let call = "::blaze_serde::serde::Deserializer::deserialize_";
        let instance = format!("__Visitor{number} {MARKERS}");
        match fields.style {
            Style::Unit => out
                .s(&format!("{call}unit_struct(__deserializer,"))
                .str(&name)
                .s(&format!(", {instance})")),
            Style::Tuple if fields.list.len() == 1 => out
                .s(&format!("{call}newtype_struct(__deserializer,"))
                .str(&name)
                .s(&format!(", {instance})")),
            Style::Tuple => out
                .s(&format!("{call}tuple_struct(__deserializer,"))
                .str(&name)
                .s(",")
                .usize(fields.list.iter().filter(|it| !it.attrs.skip).count())
                .s(&format!(", {instance})")),
            Style::Named if has_flatten(fields) => {
                out.s(&format!("{call}map(__deserializer, {instance})"))
            }
            Style::Named => out
                .s(&format!("{call}struct(__deserializer,"))
                .str(&name)
                .s(&format!(", __FIELDS{number}, {instance})")),
        };
        out.finish()
    }

    fn variants_const(out: &mut Out, variants: &[Variant]) {
        out.s("const __VARIANTS: &[&str] = &");
        out.group(proc_macro::Delimiter::Bracket, |out| {
            for variant in variants {
                out.str(&variant.key).s(",");
            }
        });
        out.s(";");
    }

    fn externally_tagged(&mut self, variants: &'i [Variant]) -> TokenStream {
        let item = self.item;
        let name = item.name();
        let deny = item.attrs.deny_unknown_fields;
        let mut out = Out::default();
        Self::variants_const(&mut out, variants);
        let mut arms = Out::default();
        for (index, variant) in variants.iter().enumerate() {
            match index + 1 == variants.len() {
                true => arms.s("_ =>"),
                false => arms.usize(index).s("=>"),
            };
            let access = format!("{DE}::VariantAccess");
            match variant.fields.style {
                Style::Unit => {
                    arms.brace(|arms| {
                        arms.s(&format!("{access}::unit_variant(__variant)?; {OK}("))
                            .ident(&item.ident)
                            .s("::")
                            .ident(&variant.ident)
                            .s(")");
                    });
                }
                Style::Tuple if variant.fields.list.len() == 1 => {
                    let store = self.store(&variant.fields.list[0]);
                    arms.s(&format!("{access}::newtype_variant::<"))
                        .tokens(&store.ty)
                        .s(">(__variant).map(|__v|");
                    arms.ident(&item.ident).s("::").ident(&variant.ident).s(if store.wrapped {
                        "(__v.value)),"
                    } else {
                        "(__v)),"
                    });
                }
                Style::Tuple | Style::Named => {
                    let ctor = Ctor {
                        variant: Some(variant),
                        fields: &variant.fields,
                        expecting: variant_expecting(&name, variant),
                    };
                    let (visitor, number) = self.visitor(&ctor, deny, None);
                    out.tokens(&visitor);
                    let instance = format!("__Visitor{number} {MARKERS}");
                    match variant.fields.style {
                        Style::Tuple => arms
                            .s(&format!("{access}::tuple_variant(__variant,"))
                            .usize(variant.fields.list.iter().filter(|it| !it.attrs.skip).count())
                            .s(&format!(", {instance}),")),
                        _ => arms.s(&format!(
                            "{access}::struct_variant(__variant, __FIELDS{number}, {instance}),"
                        )),
                    };
                }
            }
        }
        let arms = arms.finish();
        self.visitor_head(&mut out, "__Visitor");
        out.brace(|out| {
            self.visitor_items(out, &format!("enum {name}"));
            out.s(&format!("fn visit_enum<__E>(self, __data: __E) -> ::core::result::Result<Self::Value, __E::Error> where __E: {DE}::EnumAccess<'de>"));
            out.brace(|out| {
                out.s(&format!(
                    "let (__index, __variant) = {DE}::EnumAccess::variant_seed(__data, ::blaze_serde::__private::VariantSeed {{ names: __VARIANTS, other: {NONE} }})?;"
                ));
                out.s("match __index").brace(|out| {
                    out.tokens(&arms);
                });
            });
        });
        out.s("::blaze_serde::serde::Deserializer::deserialize_enum(__deserializer,")
            .str(&name)
            .s(&format!(", __VARIANTS, __Visitor {MARKERS})"));
        out.finish()
    }

    fn internally_tagged(&mut self, variants: &'i [Variant], tag: &str) -> TokenStream {
        let item = self.item;
        let name = item.name();
        let deny = item.attrs.deny_unknown_fields;
        let mut out = Out::default();
        Self::variants_const(&mut out, variants);
        let mut arms = Out::default();
        for (index, variant) in variants.iter().enumerate() {
            match index + 1 == variants.len() {
                true => arms.s("_ =>"),
                false => arms.usize(index).s("=>"),
            };
            if variant.attrs.other {
                arms.s(&format!("{OK}(")).ident(&item.ident).s("::").ident(&variant.ident).s("),");
                continue;
            }
            match variant.fields.style {
                Style::Unit => {
                    arms.brace(|arms| {
                        arms.s("::blaze_serde::__private::tagged_unit(__inner,")
                            .str(&name)
                            .s(",")
                            .str(&variant.key)
                            .s(&format!(")?; {OK}("));
                        arms.ident(&item.ident).s("::").ident(&variant.ident).s(")");
                    });
                }
                Style::Tuple => {
                    let store = self.store(&variant.fields.list[0]);
                    arms.s("<")
                        .tokens(&store.ty)
                        .s("as ::blaze_serde::Deserialize<'de>>::deserialize(__inner).map(|__v|");
                    arms.ident(&item.ident).s("::").ident(&variant.ident).s(if store.wrapped {
                        "(__v.value)),"
                    } else {
                        "(__v)),"
                    });
                }
                Style::Named => {
                    let ctor = Ctor {
                        variant: Some(variant),
                        fields: &variant.fields,
                        expecting: variant_expecting(&name, variant),
                    };
                    let (visitor, number) = self.visitor(&ctor, deny, None);
                    out.tokens(&visitor);
                    arms.s(&format!("::blaze_serde::serde::Deserializer::deserialize_any(__inner, __Visitor{number} {MARKERS}),"));
                }
            }
        }
        let other = match variants.iter().position(|it| it.attrs.other) {
            Some(index) => format!("{SOME}({index})"),
            None => NONE.to_owned(),
        };
        out.s(
            "let __human = ::blaze_serde::serde::Deserializer::is_human_readable(&__deserializer);",
        );
        out.s("let (__index, __content) = ::blaze_serde::__private::tagged(__deserializer,")
            .str(&name)
            .s(",")
            .str(tag)
            .s(&format!(", __VARIANTS, {other})?;"));
        out.s("let __inner = ::blaze_serde::__private::ContentRefDeserializer::<__D::Error>::new(&__content, __human);");
        out.s("match __index").brace(|out| {
            out.tokens(&arms.finish());
        });
        out.finish()
    }

    fn untagged(&mut self, variants: &'i [Variant]) -> TokenStream {
        let item = self.item;
        let name = item.name();
        let mut out = Out::default();
        let mut tries = Out::default();
        let inner = "::blaze_serde::__private::ContentRefDeserializer::<__D::Error>::new(&__content, __human)";
        for variant in variants {
            match variant.fields.style {
                Style::Unit => {
                    tries
                        .s(&format!(
                            "if let {OK}(()) = ::blaze_serde::__private::untagged_unit({inner})"
                        ))
                        .brace(|out| {
                            out.s(&format!("return {OK}("))
                                .ident(&item.ident)
                                .s("::")
                                .ident(&variant.ident)
                                .s(");");
                        });
                }
                Style::Tuple if variant.fields.list.len() == 1 => {
                    let store = self.store(&variant.fields.list[0]);
                    tries
                        .s(&format!("if let {OK}(__v) = <"))
                        .tokens(&store.ty)
                        .s(&format!("as ::blaze_serde::Deserialize<'de>>::deserialize({inner})"));
                    tries.brace(|out| {
                        out.s(&format!("return {OK}("))
                            .ident(&item.ident)
                            .s("::")
                            .ident(&variant.ident)
                            .s(if store.wrapped { "(__v.value));" } else { "(__v));" });
                    });
                }
                Style::Tuple | Style::Named => {
                    let ctor = Ctor {
                        variant: Some(variant),
                        fields: &variant.fields,
                        expecting: variant_expecting(&name, variant),
                    };
                    let (visitor, number) = self.visitor(&ctor, false, None);
                    out.tokens(&visitor);
                    let instance = format!("__Visitor{number} {MARKERS}");
                    let call = match variant.fields.style {
                        Style::Tuple => {
                            let len =
                                variant.fields.list.iter().filter(|it| !it.attrs.skip).count();
                            format!(
                                "::blaze_serde::serde::Deserializer::deserialize_tuple({inner}, {len}, {instance})"
                            )
                        }
                        _ => format!(
                            "::blaze_serde::serde::Deserializer::deserialize_any({inner}, {instance})"
                        ),
                    };
                    tries.s(&format!("if let {OK}(__v) = {call} {{ return {OK}(__v); }}"));
                }
            }
        }
        out.s(
            "let __human = ::blaze_serde::serde::Deserializer::is_human_readable(&__deserializer);",
        );
        out.s("let __content = <::blaze_serde::__private::Content<'de> as ::blaze_serde::Deserialize<'de>>::deserialize(__deserializer)?;");
        out.tokens(&tries.finish());
        out.s("::core::result::Result::Err(::blaze_serde::__private::no_variant::<__D::Error>(")
            .str(&name)
            .s("))");
        out.finish()
    }

    /// How each field of `fields` is read, its wrapper made when it has one.
    fn stores(&mut self, fields: &Fields) -> Vec<Store> {
        fields.list.iter().map(|it| self.store(it)).collect()
    }

    fn store(&mut self, field: &Field) -> Store {
        if field.attrs.skip {
            return Store { ty: field.ty.clone(), wrapped: false };
        }
        let path = match (&field.attrs.deserialize_with, field.attrs.borrow_cow) {
            (Some(path), _) => path.clone(),
            (None, Some(CowOf::Str)) => text("::blaze_serde::__private::borrow_cow_str"),
            (None, Some(CowOf::Bytes)) => text("::blaze_serde::__private::borrow_cow_bytes"),
            (None, None) => return Store { ty: field.ty.clone(), wrapped: false },
        };
        let number = self.wrappers.len();
        self.wrappers.push((field.ty.clone(), path));
        let mut ty = Out::default();
        ty.s(&format!("__DeserializeWith{number}<'de,"));
        crate::ser::write_arg_list(&mut ty, self.item);
        ty.s(">");
        Store { ty: ty.finish(), wrapped: true }
    }

    fn write_wrappers(&self, out: &mut Out) {
        let item = self.item;
        for (number, (ty, path)) in self.wrappers.iter().enumerate() {
            let ident = Ident::new(&format!("__DeserializeWith{number}"), Span::call_site());
            out.s("struct").ident(&ident);
            item.generics.write_impl(out, &["'de"]);
            item.generics.write_where(out, &[]);
            out.brace(|out| {
                out.s("value:")
                    .tokens(ty)
                    .s(", __marker: ::core::marker::PhantomData<fn() ->")
                    .ident(&item.ident);
                item.generics.write_args(out);
                out.s(">, __lifetime: ::core::marker::PhantomData<&'de ()>,");
            });
            out.s("impl");
            item.generics.write_impl(out, &[&self.lead]);
            out.s("::blaze_serde::Deserialize<'de> for").ident(&ident).s("<'de,");
            crate::ser::write_arg_list(out, item);
            out.s(">");
            item.generics.write_where(out, &self.bounds);
            out.brace(|out| {
                out.s("fn deserialize<__D>(__deserializer: __D) -> ::core::result::Result<Self, __D::Error> where __D: ::blaze_serde::serde::Deserializer<'de>");
                out.brace(|out| {
                    out.s(&format!("{OK}(")).ident(&ident).s("{ value:").tokens(path).s("(__deserializer)?, __marker: ::core::marker::PhantomData, __lifetime: ::core::marker::PhantomData })");
                });
            });
        }
    }

    /// `struct name<'de, ..> { markers } impl<'de, ..> Visitor<'de> for name<'de, ..> where ..`,
    /// up to the impl's body.
    fn visitor_head(&self, out: &mut Out, name: &str) {
        let item = self.item;
        let ident = Ident::new(name, Span::call_site());
        out.s("struct").ident(&ident);
        item.generics.write_impl(out, &["'de"]);
        item.generics.write_where(out, &[]);
        out.brace(|out| {
            out.s("__marker: ::core::marker::PhantomData<fn() ->").ident(&item.ident);
            item.generics.write_args(out);
            out.s(">, __lifetime: ::core::marker::PhantomData<&'de ()>,");
        });
        out.s("impl");
        item.generics.write_impl(out, &[&self.lead]);
        out.s(&format!("{DE}::Visitor<'de> for")).ident(&ident).s("<'de,");
        crate::ser::write_arg_list(out, item);
        out.s(">");
        item.generics.write_where(out, &self.bounds);
    }

    /// `type Value` and `expecting` of a visitor.
    fn visitor_items(&self, out: &mut Out, expecting: &str) {
        out.s("type Value =").ident(&self.item.ident);
        self.item.generics.write_args(out);
        out.s(";");
        out.s("fn expecting(&self, __formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result");
        out.brace(|out| {
            out.s("::core::fmt::Formatter::write_str(__formatter,").str(expecting).s(")");
        });
    }

    /// The visitor of `ctor`'s fields (`__FIELDS{n}` with it, for named fields), and its number.
    fn visitor(
        &mut self,
        ctor: &Ctor<'_>,
        deny: bool,
        container_default: Option<&DefaultOf>,
    ) -> (TokenStream, usize) {
        let number = self.visitors;
        self.visitors += 1;
        let stores = self.stores(ctor.fields);
        let fields = ctor.fields;
        let read: Vec<(&Field, &Store)> = fields
            .list
            .iter()
            .zip(&stores)
            .filter(|(it, _)| !it.attrs.skip && !it.attrs.flatten)
            .collect();
        let mut out = Out::default();
        if fields.style == Style::Named {
            out.s(&format!("const __FIELDS{number}: &[&str] = &"));
            out.group(proc_macro::Delimiter::Bracket, |out| {
                for (field, _) in &read {
                    out.str(&field.key).s(",");
                }
            });
            out.s(";");
        }
        let name = format!("__Visitor{number}");
        self.visitor_head(&mut out, &name);
        let item = self.item;
        out.brace(|out| {
            self.visitor_items(out, &ctor.expecting);
            match fields.style {
                Style::Unit => {
                    out.s(&format!("fn visit_unit<__E>(self) -> ::core::result::Result<Self::Value, __E> where __E: {DE}::Error"));
                    out.brace(|out| {
                        out.s(OK).paren(|out| construct(out, item, ctor));
                    });
                }
                Style::Tuple => {
                    if fields.list.len() == 1 && ctor.variant.is_none() {
                        let store = &stores[0];
                        out.s(
                            "fn visit_newtype_struct<__E>(self, __e: __E) -> ::core::result::Result<Self::Value, __E::Error> where __E: ::blaze_serde::serde::Deserializer<'de>",
                        );
                        out.brace(|out| {
                            out.s("let __f0 = <").tokens(&store.ty).s("as ::blaze_serde::Deserialize<'de>>::deserialize(__e)?");
                            if store.wrapped {
                                out.s(".value");
                            }
                            out.s(&format!("; {OK}")).paren(|out| construct(out, item, ctor));
                        });
                    }
                    visit_seq(out, item, ctor, &stores, None);
                }
                Style::Named => {
                    if !has_flatten(fields) {
                        visit_seq(out, item, ctor, &stores, container_default);
                    }
                    visit_map(out, item, ctor, &stores, number, deny, container_default);
                }
            }
        });
        (out.finish(), number)
    }
}

fn has_flatten(fields: &Fields) -> bool {
    fields.list.iter().any(|it| it.attrs.flatten)
}

fn variant_expecting(name: &str, variant: &Variant) -> String {
    let kind = match variant.fields.style {
        Style::Named => "struct",
        Style::Tuple => "tuple",
        Style::Unit => "unit",
    };
    format!("{kind} variant {name}::{}", unraw(&variant.ident))
}

fn text(source: &str) -> TokenStream {
    let mut out = Out::default();
    out.s(source);
    out.finish()
}

/// The value a missing or skipped `field` takes: its own default, else the container's, else
/// `Default::default()`.
fn default_of(out: &mut Out, field: &Field, container_default: Option<&DefaultOf>) {
    match (&field.attrs.default, container_default, &field.ident) {
        (Some(DefaultOf::Path(path)), _, _) => {
            out.tokens(path).s("()");
        }
        (None, Some(_), Some(ident)) => {
            out.s("__default.").ident(ident);
        }
        _ => {
            out.s("::core::default::Default::default()");
        }
    }
}

/// Whether a missing `field` takes a default rather than being an error.
fn defaults(field: &Field, container_default: Option<&DefaultOf>) -> bool {
    field.attrs.skip || field.attrs.default.is_some() || container_default.is_some()
}

/// `let __default: Self::Value = ..;` when the container has a default.
fn container_default_let(out: &mut Out, container_default: Option<&DefaultOf>) {
    match container_default {
        None => {}
        Some(DefaultOf::Trait) => {
            out.s("let __default: Self::Value = ::core::default::Default::default();");
        }
        Some(DefaultOf::Path(path)) => {
            out.s("let __default: Self::Value =").tokens(path).s("();");
        }
    }
}

/// The struct or variant of `ctor` from `__f{i}`, one per field.
fn construct(out: &mut Out, item: &Item, ctor: &Ctor<'_>) {
    out.ident(&item.ident);
    if let Some(variant) = ctor.variant {
        out.s("::").ident(&variant.ident);
    }
    match ctor.fields.style {
        Style::Unit => {}
        Style::Tuple => {
            out.paren(|out| {
                for field in &ctor.fields.list {
                    out.s(&format!("__f{},", field.index));
                }
            });
        }
        Style::Named => {
            out.brace(|out| {
                for field in &ctor.fields.list {
                    if let Some(ident) = &field.ident {
                        out.ident(ident).s(&format!(": __f{},", field.index));
                    }
                }
            });
        }
    }
}

fn visit_seq(
    out: &mut Out,
    item: &Item,
    ctor: &Ctor<'_>,
    stores: &[Store],
    container_default: Option<&DefaultOf>,
) {
    let fields = ctor.fields;
    let expected = format!(
        "{} with {} elements",
        ctor.expecting,
        fields.list.iter().filter(|it| !it.attrs.skip).count()
    );
    out.s(&format!("fn visit_seq<__A>(self, mut __seq: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {DE}::SeqAccess<'de>"));
    out.brace(|out| {
        container_default_let(out, container_default);
        let mut read = 0usize;
        for (field, store) in fields.list.iter().zip(stores) {
            out.s(&format!("let __f{} =", field.index));
            if field.attrs.skip {
                default_of(out, field, container_default);
                out.s(";");
                continue;
            }
            out.s(&format!("match {DE}::SeqAccess::next_element::<"))
                .tokens(&store.ty)
                .s(">(&mut __seq)?");
            out.brace(|out| {
                out.s(&format!("{SOME}(__v) => __v"));
                if store.wrapped {
                    out.s(".value");
                }
                out.s(&format!(", {NONE} =>"));
                match defaults(field, container_default) {
                    true => default_of(out, field, container_default),
                    false => {
                        out.s(&format!(
                            "return ::core::result::Result::Err({DE}::Error::invalid_length("
                        ))
                        .usize(read)
                        .s(", &")
                        .str(&expected)
                        .s("))");
                    }
                }
                out.s(",");
            });
            out.s(";");
            read += 1;
        }
        out.s(OK).paren(|out| construct(out, item, ctor));
    });
}

fn visit_map(
    out: &mut Out,
    item: &Item,
    ctor: &Ctor<'_>,
    stores: &[Store],
    number: usize,
    deny: bool,
    container_default: Option<&DefaultOf>,
) {
    let fields = ctor.fields;
    let flatten = has_flatten(fields);
    out.s(&format!("fn visit_map<__A>(self, mut __map: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {DE}::MapAccess<'de>"));
    out.brace(|out| {
        container_default_let(out, container_default);
        let read: Vec<(&Field, &Store)> =
            fields.list.iter().zip(stores).filter(|(it, _)| !it.attrs.skip && !it.attrs.flatten).collect();
        for (field, store) in &read {
            out.s(&format!("let mut __f{}: ::core::option::Option<", field.index)).tokens(&store.ty).s(&format!("> = {NONE};"));
        }
        if flatten {
            out.s("let mut __collect = ::blaze_serde::__private::Collect::<'de>::new();");
        }
        let seed = match flatten {
            true => format!("::blaze_serde::__private::KeySeed {{ names: __FIELDS{number} }}"),
            false => format!("::blaze_serde::__private::FieldSeed {{ names: __FIELDS{number}, deny: {deny} }}"),
        };
        out.s(&format!("while let {SOME}(__key) = {DE}::MapAccess::next_key_seed(&mut __map, {seed})?"));
        out.brace(|out| {
            out.s("match __key").brace(|out| {
                for (slot, (field, store)) in read.iter().enumerate() {
                    match (flatten, slot + 1 == read.len()) {
                        (true, true) => out.s("::blaze_serde::__private::Key::Field(_) =>"),
                        (true, false) => out.s("::blaze_serde::__private::Key::Field(").usize(slot).s(") =>"),
                        (false, _) => out.s(&format!("{SOME}(")).usize(slot).s(") =>"),
                    };
                    out.brace(|out| {
                        out.s(&format!("if __f{}.is_some()", field.index)).brace(|out| {
                            out.s(&format!("return ::core::result::Result::Err(<__A::Error as {DE}::Error>::duplicate_field(")).str(&field.key).s("));");
                        });
                        out.s(&format!("__f{} = {SOME}({DE}::MapAccess::next_value::<", field.index)).tokens(&store.ty).s(">(&mut __map)?);");
                    });
                }
                match flatten {
                    true => {
                        if read.is_empty() {
                            out.s("::blaze_serde::__private::Key::Field(_) => {}");
                        }
                        out.s(&format!(
                            "::blaze_serde::__private::Key::Other(__name) => {{ __collect.push(__name, {DE}::MapAccess::next_value::<::blaze_serde::__private::Content<'de>>(&mut __map)?); }}"
                        ));
                    }
                    false => {
                        out.s(&format!("_ => {{ {DE}::MapAccess::next_value::<{DE}::IgnoredAny>(&mut __map)?; }}"));
                    }
                }
            });
        });
        for (field, store) in fields.list.iter().zip(stores) {
            out.s(&format!("let __f{} =", field.index));
            if field.attrs.skip {
                default_of(out, field, container_default);
            } else if field.attrs.flatten {
                out.s("<").tokens(&field.ty).s("as ::blaze_serde::Deserialize<'de>>::deserialize(__collect.deserializer::<__A::Error>())?");
            } else {
                out.s(&format!("match __f{}", field.index)).brace(|out| {
                    out.s(&format!("{SOME}(__v) => __v"));
                    if store.wrapped {
                        out.s(".value");
                    }
                    out.s(&format!(", {NONE} =>"));
                    if defaults(field, container_default) {
                        default_of(out, field, container_default);
                    } else if store.wrapped {
                        out.s(&format!("return ::core::result::Result::Err(<__A::Error as {DE}::Error>::missing_field(")).str(&field.key).s("))");
                    } else {
                        out.s("::blaze_serde::__private::missing_field::<_, __A::Error>(").str(&field.key).s(")?");
                    }
                    out.s(",");
                });
            }
            out.s(";");
        }
        out.s(OK).paren(|out| construct(out, item, ctor));
    });
}
