//! `impl Serialize`: `serialize_struct` and its fields for a struct, a `serialize_*_variant`
//! (or the tagged or untagged shape) for each variant of an enum.

use proc_macro::{Ident, TokenStream};

use crate::{
    generics,
    out::Out,
    parse::{Data, Error, Field, Fields, Item, Style, Variant, at},
};

pub(crate) fn derive(item: &Item) -> Result<TokenStream, Error> {
    check(item)?;
    let mut out = Out::default();
    out.s("#[automatically_derived] impl");
    item.generics.write_impl(&mut out, &[]);
    out.s("::blaze_serde::Serialize for").ident(&item.ident);
    item.generics.write_args(&mut out);
    item.generics.write_where(&mut out, &bounds(item));
    out.brace(|out| {
        out.s("fn serialize<__S>(&self, __serializer: __S) -> ::core::result::Result<__S::Ok, __S::Error> where __S: ::blaze_serde::serde::Serializer");
        out.brace(|out| body(out, item));
    });
    Ok(out.finish())
}

/// What a `Serialize` refuses that `Deserialize` takes.
fn check(item: &Item) -> Result<(), Error> {
    let refuse = |what: &str| -> Result<(), Error> {
        let span = item.ident.span();
        Err(Error::new(span, format!("`{}` ({}): {what}", item.name(), at(span))))
    };
    let tuple_skip_if = |fields: &Fields| {
        fields.style == Style::Tuple
            && fields.list.iter().any(|it| it.attrs.skip_serializing_if.is_some())
    };
    match &item.data {
        Data::Struct(fields) => {
            if tuple_skip_if(fields) {
                return refuse(
                    "`#[serde(skip_serializing_if)]` on a tuple field is not supported by blaze-serde-derive",
                );
            }
        }
        Data::Enum(variants) => {
            for variant in variants {
                if variant.fields.list.iter().any(|it| it.attrs.flatten) {
                    return refuse(
                        "`#[serde(flatten)]` in a variant is not supported by blaze-serde-derive's `Serialize`",
                    );
                }
                if tuple_skip_if(&variant.fields) {
                    return refuse(
                        "`#[serde(skip_serializing_if)]` on a tuple field is not supported by blaze-serde-derive",
                    );
                }
            }
        }
    }
    Ok(())
}

/// `T: Serialize` for each type parameter a serialized field names, or `bound(serialize)`.
fn bounds(item: &Item) -> Vec<TokenStream> {
    if let Some(bound) = &item.attrs.bound_serialize {
        return bound.clone();
    }
    let fields: Vec<&Field> = match &item.data {
        Data::Struct(fields) => fields.list.iter().collect(),
        Data::Enum(variants) => variants.iter().flat_map(|it| &it.fields.list).collect(),
    };
    item.generics
        .type_params()
        .filter(|param| {
            fields.iter().any(|it| {
                !it.attrs.skip
                    && it.attrs.serialize_with.is_none()
                    && generics::mentions(&it.ty, param)
            })
        })
        .map(|param| {
            let mut out = Out::default();
            out.ident(param).s(": ::blaze_serde::Serialize");
            out.finish()
        })
        .collect()
}

/// How a field's value is reached: `self.field` of a struct, or a binding of a variant's
/// pattern (already a reference).
#[derive(Clone, Copy)]
enum Access<'f> {
    Own(&'f Field),
    Bound(usize),
}

impl Access<'_> {
    /// A reference to the value.
    fn write(self, out: &mut Out) {
        match self {
            Access::Own(field) => {
                out.s("&self.");
                match &field.ident {
                    Some(ident) => out.ident(ident),
                    None => out.usize(field.index),
                };
            }
            Access::Bound(index) => {
                out.s(&format!("__f{index}"));
            }
        }
    }
}

fn body(out: &mut Out, item: &Item) {
    let mut wrappers = Wrappers::new(item);
    let mut code = Out::default();
    match &item.data {
        Data::Struct(fields) => struct_body(&mut code, item, fields, &mut wrappers),
        Data::Enum(variants) => enum_body(&mut code, item, variants, &mut wrappers),
    }
    wrappers.write(out, item);
    out.tokens(&code.finish());
}

/// The value to serialize for `field`: the reference, or its `serialize_with` wrapper.
fn value(out: &mut Out, field: &Field, access: Access<'_>, wrappers: &mut Wrappers) {
    match &field.attrs.serialize_with {
        None => access.write(out),
        Some(path) => {
            // The wrapper redeclares the item's generics, and the field's type need not name
            // them all (`half: u8` in `With<T>`), so the arguments are written out: `'_` for
            // the borrow, then the item's own, in scope here from the impl.
            let name = wrappers.add(field, path.clone());
            out.s(&format!("&{name}::<'_,")).tokens(&wrappers.args).s("> { value:");
            access.write(out);
            out.s(", __marker: ::core::marker::PhantomData }");
        }
    }
}

/// The fields that are written at all, with their accesses.
fn written<'f>(
    fields: &'f Fields,
    access: impl Fn(&'f Field) -> Access<'f>,
) -> Vec<(&'f Field, Access<'f>)> {
    fields.list.iter().filter(|it| !it.attrs.skip).map(|it| (it, access(it))).collect()
}

/// The number of fields written: `n`, plus `if skip(&v) { 0 } else { 1 }` for each field with
/// `skip_serializing_if`.
fn length(out: &mut Out, fields: &[(&Field, Access<'_>)], plus: usize) {
    let fixed =
        fields.iter().filter(|(it, _)| it.attrs.skip_serializing_if.is_none()).count() + plus;
    out.usize(fixed);
    for (field, access) in fields {
        if let Some(path) = &field.attrs.skip_serializing_if {
            out.s("+ if").tokens(path).paren(|out| access.write(out)).s("{ 0 } else { 1 }");
        }
    }
}

/// `trait_::serialize_field(&mut __state, "key", value)?;` for each field (no key for a
/// tuple's), skipped by its `skip_serializing_if` with `skip_field` for a struct's.
fn fields_of(
    out: &mut Out,
    fields: &[(&Field, Access<'_>)],
    trait_: &str,
    keyed: bool,
    wrappers: &mut Wrappers,
) {
    let skip_field = keyed;
    for (field, access) in fields {
        let mut write = Out::default();
        write.s(&format!("{trait_}::serialize_field(&mut __state,"));
        if keyed {
            write.str(&field.key).s(",");
        }
        value(&mut write, field, *access, wrappers);
        write.s(")?;");
        let write = write.finish();
        match &field.attrs.skip_serializing_if {
            None => {
                out.tokens(&write);
            }
            Some(path) => {
                out.s("if !").tokens(path).paren(|out| access.write(out)).brace(|out| {
                    out.tokens(&write);
                });
                if skip_field {
                    out.s("else").brace(|out| {
                        out.s(&format!("{trait_}::skip_field(&mut __state,"))
                            .str(&field.key)
                            .s(")?;");
                    });
                }
            }
        }
    }
}

const SER: &str = "::blaze_serde::serde::ser";

fn struct_body(out: &mut Out, item: &Item, fields: &Fields, wrappers: &mut Wrappers) {
    let name = item.name();
    let own = written(fields, Access::Own);
    if item.attrs.transparent {
        if let Some((field, access)) = own.first() {
            match &field.attrs.serialize_with {
                None => {
                    out.s("::blaze_serde::Serialize::serialize(");
                    access.write(out);
                    out.s(", __serializer)");
                }
                Some(path) => {
                    out.tokens(path).s("(");
                    access.write(out);
                    out.s(", __serializer)");
                }
            }
        }
        return;
    }
    match fields.style {
        Style::Unit => {
            out.s(&format!("{SER}::Serializer::serialize_unit_struct(__serializer,"))
                .str(&name)
                .s(")");
        }
        Style::Tuple if fields.list.len() == 1 && own.len() == 1 => {
            out.s(&format!("{SER}::Serializer::serialize_newtype_struct(__serializer,"))
                .str(&name)
                .s(",");
            value(out, own[0].0, own[0].1, wrappers);
            out.s(")");
        }
        Style::Tuple => {
            out.s(&format!(
                "let mut __state = {SER}::Serializer::serialize_tuple_struct(__serializer,"
            ))
            .str(&name)
            .s(",");
            length(out, &own, 0);
            out.s(")?;");
            fields_of(out, &own, &format!("{SER}::SerializeTupleStruct"), false, wrappers);
            out.s(&format!("{SER}::SerializeTupleStruct::end(__state)"));
        }
        Style::Named if own.iter().any(|(it, _)| it.attrs.flatten) => {
            out.s(&format!("let mut __state = {SER}::Serializer::serialize_map(__serializer, ::core::option::Option::None)?;"));
            for (field, access) in &own {
                if field.attrs.flatten {
                    out.s("::blaze_serde::Serialize::serialize(");
                    access.write(out);
                    out.s(", ::blaze_serde::__private::FlatMapSerializer(&mut __state))?;");
                    continue;
                }
                let mut write = Out::default();
                write
                    .s(&format!("{SER}::SerializeMap::serialize_entry(&mut __state,"))
                    .str(&field.key)
                    .s(",");
                value(&mut write, field, *access, wrappers);
                write.s(")?;");
                let write = write.finish();
                match &field.attrs.skip_serializing_if {
                    None => out.tokens(&write),
                    Some(path) => {
                        out.s("if !").tokens(path).paren(|out| access.write(out)).brace(|out| {
                            out.tokens(&write);
                        })
                    }
                };
            }
            out.s(&format!("{SER}::SerializeMap::end(__state)"));
        }
        Style::Named => {
            out.s(&format!("let mut __state = {SER}::Serializer::serialize_struct(__serializer,"))
                .str(&name)
                .s(",");
            length(out, &own, 0);
            out.s(")?;");
            fields_of(out, &own, &format!("{SER}::SerializeStruct"), true, wrappers);
            out.s(&format!("{SER}::SerializeStruct::end(__state)"));
        }
    }
}

/// The pattern of `variant` binding each field to `__f{i}`.
fn pattern(out: &mut Out, item: &Item, variant: &Variant) {
    out.ident(&item.ident).s("::").ident(&variant.ident);
    match variant.fields.style {
        Style::Unit => {}
        Style::Tuple => {
            out.paren(|out| {
                for field in &variant.fields.list {
                    out.s(&format!("ref __f{},", field.index));
                }
            });
        }
        Style::Named => {
            out.brace(|out| {
                for field in &variant.fields.list {
                    if let Some(ident) = &field.ident {
                        out.ident(ident).s(&format!(": ref __f{},", field.index));
                    }
                }
            });
        }
    }
}

fn enum_body(out: &mut Out, item: &Item, variants: &[Variant], wrappers: &mut Wrappers) {
    let name = item.name();
    // `*self` so an enum with no variants matches with no arms; each arm binds its fields by
    // reference through `ref`.
    out.s("match *self");
    out.brace(|out| {
        for (index, variant) in (0u32..).zip(variants) {
            pattern(out, item, variant);
            out.s("=>");
            out.brace(|out| variant_body(out, item, &name, index, variant, wrappers));
        }
    });
}

fn variant_body(
    out: &mut Out,
    item: &Item,
    name: &str,
    index: u32,
    variant: &Variant,
    wrappers: &mut Wrappers,
) {
    let fields = written(&variant.fields, |it| Access::Bound(it.index));
    let key = variant.key.as_str();
    let variant_name = crate::parse::unraw(&variant.ident);
    if let Some(tag) = &item.attrs.tag {
        match variant.fields.style {
            Style::Unit => {
                out.s(&format!(
                    "let mut __state = {SER}::Serializer::serialize_struct(__serializer,"
                ))
                .str(name)
                .s(", 1)?;");
                out.s(&format!("{SER}::SerializeStruct::serialize_field(&mut __state,"))
                    .str(tag)
                    .s(",")
                    .str(key)
                    .s(")?;");
                out.s(&format!("{SER}::SerializeStruct::end(__state)"));
            }
            Style::Tuple => {
                out.s("::blaze_serde::__private::serialize_tagged_newtype(__serializer,")
                    .str(name)
                    .s(",")
                    .str(&variant_name)
                    .s(",");
                out.str(tag).s(",").str(key).s(",");
                match fields.first() {
                    Some((field, access)) => value(out, field, *access, wrappers),
                    None => {
                        out.s("&()");
                    }
                }
                out.s(")");
            }
            Style::Named => {
                out.s(&format!(
                    "let mut __state = {SER}::Serializer::serialize_struct(__serializer,"
                ))
                .str(name)
                .s(",");
                length(out, &fields, 1);
                out.s(")?;");
                out.s(&format!("{SER}::SerializeStruct::serialize_field(&mut __state,"))
                    .str(tag)
                    .s(",")
                    .str(key)
                    .s(")?;");
                fields_of(out, &fields, &format!("{SER}::SerializeStruct"), true, wrappers);
                out.s(&format!("{SER}::SerializeStruct::end(__state)"));
            }
        }
        return;
    }
    if item.attrs.untagged {
        match variant.fields.style {
            Style::Unit => {
                out.s(&format!("{SER}::Serializer::serialize_unit(__serializer)"));
            }
            Style::Tuple if variant.fields.list.len() == 1 && fields.len() == 1 => {
                out.s("::blaze_serde::Serialize::serialize(");
                value(out, fields[0].0, fields[0].1, wrappers);
                out.s(", __serializer)");
            }
            Style::Tuple => {
                out.s(&format!(
                    "let mut __state = {SER}::Serializer::serialize_tuple(__serializer,"
                ));
                length(out, &fields, 0);
                out.s(")?;");
                for (field, access) in &fields {
                    out.s(&format!("{SER}::SerializeTuple::serialize_element(&mut __state,"));
                    value(out, field, *access, wrappers);
                    out.s(")?;");
                }
                out.s(&format!("{SER}::SerializeTuple::end(__state)"));
            }
            Style::Named => {
                out.s(&format!(
                    "let mut __state = {SER}::Serializer::serialize_struct(__serializer,"
                ))
                .str(&variant_name)
                .s(",");
                length(out, &fields, 0);
                out.s(")?;");
                fields_of(out, &fields, &format!("{SER}::SerializeStruct"), true, wrappers);
                out.s(&format!("{SER}::SerializeStruct::end(__state)"));
            }
        }
        return;
    }
    match variant.fields.style {
        Style::Unit => {
            out.s(&format!("{SER}::Serializer::serialize_unit_variant(__serializer,"))
                .str(name)
                .s(",")
                .u32(index)
                .s(",")
                .str(key)
                .s(")");
        }
        Style::Tuple if variant.fields.list.len() == 1 && fields.len() == 1 => {
            out.s(&format!("{SER}::Serializer::serialize_newtype_variant(__serializer,"))
                .str(name)
                .s(",")
                .u32(index)
                .s(",")
                .str(key)
                .s(",");
            value(out, fields[0].0, fields[0].1, wrappers);
            out.s(")");
        }
        Style::Tuple => {
            out.s(&format!(
                "let mut __state = {SER}::Serializer::serialize_tuple_variant(__serializer,"
            ))
            .str(name)
            .s(",")
            .u32(index)
            .s(",")
            .str(key)
            .s(",");
            length(out, &fields, 0);
            out.s(")?;");
            fields_of(out, &fields, &format!("{SER}::SerializeTupleVariant"), false, wrappers);
            out.s(&format!("{SER}::SerializeTupleVariant::end(__state)"));
        }
        Style::Named => {
            out.s(&format!(
                "let mut __state = {SER}::Serializer::serialize_struct_variant(__serializer,"
            ))
            .str(name)
            .s(",")
            .u32(index)
            .s(",")
            .str(key)
            .s(",");
            length(out, &fields, 0);
            out.s(")?;");
            fields_of(out, &fields, &format!("{SER}::SerializeStructVariant"), true, wrappers);
            out.s(&format!("{SER}::SerializeStructVariant::end(__state)"));
        }
    }
}

/// The `serialize_with` wrappers of one impl: a struct per field holding a reference to the
/// value, whose `Serialize` calls the path. Each carries the item's generics, since the
/// field's type may name them.
struct Wrappers {
    list: Vec<(String, TokenStream, TokenStream)>,
    /// The item's generic arguments without the angle brackets, which each construction
    /// passes to its wrapper.
    args: TokenStream,
}

impl Wrappers {
    fn new(item: &Item) -> Self {
        let mut args = Out::default();
        write_arg_list(&mut args, item);
        Wrappers { list: Vec::new(), args: args.finish() }
    }

    fn add(&mut self, field: &Field, path: TokenStream) -> String {
        let name = format!("__SerializeWith{}", self.list.len());
        self.list.push((name.clone(), field.ty.clone(), path));
        name
    }

    fn write(&self, out: &mut Out, item: &Item) {
        for (name, ty, path) in &self.list {
            let ident = Ident::new(name, proc_macro::Span::call_site());
            out.s("struct").ident(&ident);
            item.generics.write_impl(out, &["'__a"]);
            item.generics.write_where(out, &[]);
            out.brace(|out| {
                out.s("value: &'__a")
                    .tokens(ty)
                    .s(", __marker: ::core::marker::PhantomData<fn() ->")
                    .ident(&item.ident);
                item.generics.write_args(out);
                out.s(">,");
            });
            out.s("impl");
            item.generics.write_impl(out, &["'__a"]);
            out.s("::blaze_serde::Serialize for").ident(&ident);
            out.s("<'__a,");
            write_arg_list(out, item);
            out.s(">");
            item.generics.write_where(out, &[]);
            out.brace(|out| {
                out.s("fn serialize<__S>(&self, __serializer: __S) -> ::core::result::Result<__S::Ok, __S::Error> where __S: ::blaze_serde::serde::Serializer");
                out.brace(|out| {
                    out.tokens(path).s("(self.value, __serializer)");
                });
            });
        }
    }
}

/// The item's generic arguments without the angle brackets.
pub(crate) fn write_arg_list(out: &mut Out, item: &Item) {
    for param in &item.generics.params {
        match param {
            generics::Param::Lifetime { name, .. } => out.s(name).s(","),
            generics::Param::Type { ident, .. } | generics::Param::Const { ident, .. } => {
                out.ident(ident).s(",")
            }
        };
    }
}
