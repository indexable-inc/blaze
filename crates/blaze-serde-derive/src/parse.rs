//! The item a derive is on, read from its token trees: its name, generics, `#[serde(..)]`
//! attributes, and the fields of the struct or of each variant. Only what the impls need is
//! kept: the field types and bounds as the tokens they were written as.

use std::str::FromStr as _;

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

use crate::generics::{self, Generics};

/// A derive's refusal: `message` names the attribute or form, the item and where it is.
pub(crate) struct Error {
    span: Span,
    message: String,
}

impl Error {
    pub(crate) fn new(span: Span, message: String) -> Error {
        Error { span, message }
    }

    /// `::core::compile_error! { "message" }`, at the span refused.
    pub(crate) fn into_compile_error(self) -> TokenStream {
        let span = self.span;
        let punct = |ch: char, spacing: Spacing| {
            let mut it = Punct::new(ch, spacing);
            it.set_span(span);
            TokenTree::Punct(it)
        };
        let mut message = Literal::string(&self.message);
        message.set_span(span);
        let mut body = Group::new(Delimiter::Brace, TokenStream::from(TokenTree::Literal(message)));
        body.set_span(span);
        [
            punct(':', Spacing::Joint),
            punct(':', Spacing::Alone),
            TokenTree::Ident(Ident::new("core", span)),
            punct(':', Spacing::Joint),
            punct(':', Spacing::Alone),
            TokenTree::Ident(Ident::new("compile_error", span)),
            punct('!', Spacing::Alone),
            TokenTree::Group(body),
        ]
        .into_iter()
        .collect()
    }
}

/// Where `span` is written: `file:line:column`.
pub(crate) fn at(span: Span) -> String {
    format!("{}:{}:{}", span.file(), span.line(), span.column())
}

pub(crate) struct Item {
    pub(crate) ident: Ident,
    pub(crate) generics: Generics,
    pub(crate) attrs: Container,
    pub(crate) data: Data,
}

impl Item {
    pub(crate) fn name(&self) -> String {
        unraw(&self.ident)
    }
}

pub(crate) enum Data {
    Struct(Fields),
    Enum(Vec<Variant>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    /// `{ a: A }`
    Named,
    /// `(A, B)`
    Tuple,
    /// no fields
    Unit,
}

pub(crate) struct Fields {
    pub(crate) style: Style,
    pub(crate) list: Vec<Field>,
}

pub(crate) struct Field {
    /// The field's name, or `None` for a tuple field.
    pub(crate) ident: Option<Ident>,
    /// Its position among the fields.
    pub(crate) index: usize,
    /// Its name in the format: `rename`, else `rename_all` of its name.
    pub(crate) key: String,
    pub(crate) ty: TokenStream,
    pub(crate) attrs: FieldAttrs,
}

pub(crate) struct Variant {
    pub(crate) ident: Ident,
    /// Its name in the format, after the enum's `rename_all`.
    pub(crate) key: String,
    pub(crate) fields: Fields,
    pub(crate) attrs: VariantAttrs,
}

/// `default` or `default = "path"`.
pub(crate) enum DefaultOf {
    Trait,
    Path(TokenStream),
}

/// The container attributes read: `rename_all`, `deny_unknown_fields`, `default`,
/// `transparent`, `tag`, `untagged`, `from`, `try_from` and `bound(serialize, deserialize)`.
#[derive(Default)]
pub(crate) struct Container {
    pub(crate) rename_all: Option<RenameRule>,
    pub(crate) deny_unknown_fields: bool,
    pub(crate) default: Option<DefaultOf>,
    pub(crate) transparent: bool,
    pub(crate) tag: Option<String>,
    pub(crate) untagged: bool,
    pub(crate) from: Option<TokenStream>,
    pub(crate) try_from: Option<TokenStream>,
    pub(crate) bound_serialize: Option<Vec<TokenStream>>,
    pub(crate) bound_deserialize: Option<Vec<TokenStream>>,
}

/// The field attributes read: `rename`, `default`, `skip`, `skip_serializing_if`, `with`,
/// `serialize_with`, `deserialize_with`, `borrow` and `flatten`.
#[derive(Default)]
pub(crate) struct FieldAttrs {
    pub(crate) rename: Option<String>,
    pub(crate) default: Option<DefaultOf>,
    pub(crate) skip: bool,
    pub(crate) skip_serializing_if: Option<TokenStream>,
    pub(crate) serialize_with: Option<TokenStream>,
    pub(crate) deserialize_with: Option<TokenStream>,
    /// The lifetimes the field borrows from the input: `borrow`'s, and those of a `&str` or
    /// `&[u8]`, which serde borrows unasked.
    pub(crate) borrowed: Vec<String>,
    /// `borrow` on a `Cow<'a, str>` or `Cow<'a, [u8]>`: read through serde's borrowing
    /// readers, so it borrows when the format can.
    pub(crate) borrow_cow: Option<CowOf>,
    pub(crate) flatten: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum CowOf {
    Str,
    Bytes,
}

/// The variant attribute read: `other`.
#[derive(Default)]
pub(crate) struct VariantAttrs {
    pub(crate) other: bool,
}

/// `rename_all`'s rules that blaze's crates use.
#[derive(Clone, Copy)]
pub(crate) enum RenameRule {
    Lowercase,
    SnakeCase,
    KebabCase,
    CamelCase,
}

impl RenameRule {
    fn of(value: &str) -> Option<RenameRule> {
        Some(match value {
            "lowercase" => RenameRule::Lowercase,
            "snake_case" => RenameRule::SnakeCase,
            "kebab-case" => RenameRule::KebabCase,
            "camelCase" => RenameRule::CamelCase,
            _ => return None,
        })
    }

    /// A variant's name (`PascalCase`) under the rule, as serde's `apply_to_variant`.
    pub(crate) fn variant(self, name: &str) -> String {
        match self {
            RenameRule::Lowercase => name.to_ascii_lowercase(),
            RenameRule::CamelCase => {
                let mut chars = name.chars();
                chars
                    .next()
                    .map(|first| first.to_ascii_lowercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            }
            RenameRule::SnakeCase | RenameRule::KebabCase => {
                let mut snake = String::new();
                for (i, ch) in name.char_indices() {
                    if i > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.push(ch.to_ascii_lowercase());
                }
                match self {
                    RenameRule::KebabCase => snake.replace('_', "-"),
                    _ => snake,
                }
            }
        }
    }

    /// A field's name (`snake_case`) under the rule, as serde's `apply_to_field`.
    pub(crate) fn field(self, name: &str) -> String {
        match self {
            RenameRule::Lowercase | RenameRule::SnakeCase => name.to_owned(),
            RenameRule::KebabCase => name.replace('_', "-"),
            RenameRule::CamelCase => {
                let mut camel = String::new();
                let mut upper = false;
                for ch in name.chars() {
                    if ch == '_' {
                        upper = true;
                    } else if upper {
                        camel.push(ch.to_ascii_uppercase());
                        upper = false;
                    } else {
                        camel.push(ch);
                    }
                }
                camel
            }
        }
    }
}

/// An identifier as written, without `r#`.
pub(crate) fn unraw(ident: &Ident) -> String {
    let text = ident.to_string();
    match text.strip_prefix("r#") {
        Some(it) => it.to_owned(),
        None => text,
    }
}

/// The item of `input`, a derive's input.
pub(crate) fn item(input: TokenStream) -> Result<Item, Error> {
    let mut c = Cursor::new(input.into_iter().collect());
    let attrs = c.outer_attrs();
    c.visibility();
    let keyword = c.ident("`struct` or `enum`")?;
    let ident = c.ident("the item's name")?;
    let name = unraw(&ident);
    let mut generics = match c.peek_punct('<') {
        true => generics::parse(&name, c.angle()?)?,
        false => Generics::default(),
    };
    let what = Owner { item: &name, field: None };
    let container = container_attrs(&attrs, &what)?;
    let data = match keyword.to_string().as_str() {
        "struct" => {
            generics.predicates.extend(c.where_clause());
            let fields = match c.next() {
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => {
                    named_fields(&name, None, &group, container.rename_all)?
                }
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
                    let fields = tuple_fields(&name, None, &group)?;
                    generics.predicates.extend(c.where_clause());
                    fields
                }
                Some(TokenTree::Punct(punct)) if punct.as_char() == ';' => {
                    Fields { style: Style::Unit, list: Vec::new() }
                }
                _ => {
                    return Err(Error::new(
                        ident.span(),
                        format!(
                            "struct `{name}` ({}): its body does not read as a struct's",
                            at(ident.span())
                        ),
                    ));
                }
            };
            Data::Struct(fields)
        }
        "enum" => {
            generics.predicates.extend(c.where_clause());
            let Some(TokenTree::Group(body)) = c.next().filter(
                |it| matches!(it, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace),
            ) else {
                return Err(Error::new(
                    ident.span(),
                    format!(
                        "enum `{name}` ({}): its body does not read as an enum's",
                        at(ident.span())
                    ),
                ));
            };
            Data::Enum(variants(&name, &body, container.rename_all)?)
        }
        other => {
            return Err(Error::new(
                keyword.span(),
                format!(
                    "`{other} {name}` ({}): blaze-serde-derive derives for structs and enums only",
                    at(keyword.span())
                ),
            ));
        }
    };
    check_container(&ident, &container, &data)?;
    Ok(Item { ident, generics, attrs: container, data })
}

/// What an attribute is on, for messages: `Item`, `Item::field` or `Item::Variant`.
struct Owner<'a> {
    item: &'a str,
    field: Option<&'a str>,
}

impl std::fmt::Display for Owner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.field {
            None => write!(f, "`{}`", self.item),
            Some(field) => write!(f, "`{}::{field}`", self.item),
        }
    }
}

/// The combinations serde refuses, refused here too.
fn check_container(ident: &Ident, attrs: &Container, data: &Data) -> Result<(), Error> {
    let name = unraw(ident);
    let refuse = |what: &str| {
        Err(Error::new(ident.span(), format!("`{name}` ({}): {what}", at(ident.span()))))
    };
    match data {
        Data::Struct(fields) => {
            if attrs.tag.is_some() || attrs.untagged {
                return refuse("`#[serde(tag)]` and `#[serde(untagged)]` are for enums");
            }
            if attrs.transparent && fields.list.iter().filter(|it| !it.attrs.skip).count() != 1 {
                return refuse(
                    "`#[serde(transparent)]` needs exactly one field that is not skipped",
                );
            }
            if attrs.default.is_some() && fields.style != Style::Named {
                return refuse(
                    "`#[serde(default)]` on a container is for structs with named fields",
                );
            }
            if fields.style != Style::Named && fields.list.iter().any(|it| it.attrs.flatten) {
                return refuse("`#[serde(flatten)]` is for named fields");
            }
            if attrs.deny_unknown_fields && fields.list.iter().any(|it| it.attrs.flatten) {
                return refuse(
                    "`#[serde(flatten)]` can't be used with `#[serde(deny_unknown_fields)]`",
                );
            }
        }
        Data::Enum(variants) => {
            if attrs.transparent {
                return refuse("`#[serde(transparent)]` is for structs");
            }
            if attrs.default.is_some() {
                return refuse("`#[serde(default)]` on a container is for structs");
            }
            if attrs.tag.is_some() && attrs.untagged {
                return refuse("`#[serde(tag)]` and `#[serde(untagged)]` together");
            }
            if attrs.tag.is_some()
                && variants
                    .iter()
                    .any(|it| it.fields.style == Style::Tuple && it.fields.list.len() != 1)
            {
                return refuse("`#[serde(tag)]` can't be used with tuple variants");
            }
            let others: Vec<&Variant> = variants.iter().filter(|it| it.attrs.other).collect();
            if others.len() > 1 {
                return refuse("more than one `#[serde(other)]` variant");
            }
            if others.iter().any(|it| it.fields.style != Style::Unit) {
                return refuse("`#[serde(other)]` is for a unit variant");
            }
            if attrs.untagged && !others.is_empty() {
                return refuse("`#[serde(other)]` in an untagged enum");
            }
            for variant in variants {
                if variant.fields.list.iter().any(|it| it.attrs.flatten)
                    && variant.fields.style != Style::Named
                {
                    return refuse("`#[serde(flatten)]` is for named fields");
                }
            }
        }
    }
    if attrs.transparent && (attrs.from.is_some() || attrs.try_from.is_some()) {
        return refuse("`#[serde(transparent)]` with `from` or `try_from`");
    }
    if attrs.from.is_some() && attrs.try_from.is_some() {
        return refuse("`#[serde(from)]` and `#[serde(try_from)]` together");
    }
    Ok(())
}

/// One `key`, `key = "value"` or `key(..)` of a `#[serde(..)]` list.
struct Meta {
    key: Ident,
    value: Value,
}

enum Value {
    Flag,
    Str(String, Span),
    List(Vec<Meta>),
}

impl Meta {
    fn name(&self) -> String {
        self.key.to_string()
    }

    fn span(&self) -> Span {
        self.key.span()
    }
}

/// The `#[serde(..)]` lists of `attrs`, each `#[..]`'s bracket group, in order.
fn serde_metas(attrs: &[Group], owner: &Owner<'_>) -> Result<Vec<Meta>, Error> {
    let mut metas = Vec::new();
    for attr in attrs {
        let tokens: Vec<TokenTree> = attr.stream().into_iter().collect();
        let Some(TokenTree::Ident(head)) = tokens.first() else { continue };
        if head.to_string() != "serde" {
            continue;
        }
        let list = match tokens.get(1..) {
            Some([TokenTree::Group(list)]) if list.delimiter() == Delimiter::Parenthesis => list,
            _ => {
                return Err(Error::new(
                    head.span(),
                    format!(
                        "`#[serde ..]` on {owner} ({}): blaze-serde-derive reads `#[serde(..)]` lists only",
                        at(head.span())
                    ),
                ));
            }
        };
        metas.extend(meta_list(list.stream(), owner)?);
    }
    Ok(metas)
}

fn meta_list(stream: TokenStream, owner: &Owner<'_>) -> Result<Vec<Meta>, Error> {
    let mut metas = Vec::new();
    for chunk in split_commas(stream.into_iter().collect()) {
        let meta = match chunk.as_slice() {
            [TokenTree::Ident(key)] => Meta { key: key.clone(), value: Value::Flag },
            [TokenTree::Ident(key), TokenTree::Punct(eq), TokenTree::Literal(lit)]
                if eq.as_char() == '=' =>
            {
                let value = string_of(lit).ok_or_else(|| {
                    Error::new(
                        lit.span(),
                        format!(
                            "`#[serde({key} = {lit})]` on {owner} ({}): blaze-serde-derive reads a plain string literal here, with no escapes",
                            at(lit.span())
                        ),
                    )
                })?;
                Meta { key: key.clone(), value: Value::Str(value, lit.span()) }
            }
            [TokenTree::Ident(key), TokenTree::Group(list)]
                if list.delimiter() == Delimiter::Parenthesis =>
            {
                Meta { key: key.clone(), value: Value::List(meta_list(list.stream(), owner)?) }
            }
            _ => {
                let span = chunk.first().map_or_else(Span::call_site, TokenTree::span);
                let text: TokenStream = chunk.iter().cloned().collect();
                return Err(Error::new(
                    span,
                    format!(
                        "`#[serde({text})]` on {owner} ({}): a form blaze-serde-derive does not read",
                        at(span)
                    ),
                ));
            }
        };
        metas.push(meta);
    }
    Ok(metas)
}

/// The text of a plain string literal: `"text"` with no escape and no raw prefix.
fn string_of(lit: &Literal) -> Option<String> {
    let text = lit.to_string();
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\')).then(|| inner.to_owned())
}

/// The error for an attribute `meta` that `owner`, of the kind `place`, does not take.
fn unsupported(meta: &Meta, owner: &Owner<'_>, place: &str) -> Error {
    let form = match &meta.value {
        Value::Flag => meta.name(),
        Value::Str(value, _) => format!("{} = \"{value}\"", meta.name()),
        Value::List(_) => format!("{}(..)", meta.name()),
    };
    Error::new(
        meta.span(),
        format!(
            "`#[serde({form})]` on {place} {owner} ({}): blaze-serde-derive does not support it there; it reads only the attributes blaze's crates use",
            at(meta.span())
        ),
    )
}

fn duplicate(meta: &Meta, owner: &Owner<'_>) -> Error {
    Error::new(
        meta.span(),
        format!("`#[serde({})]` twice on {owner} ({})", meta.name(), at(meta.span())),
    )
}

/// `value` of `meta`, which must be `key = "value"`.
fn str_value<'m>(meta: &'m Meta, owner: &Owner<'_>, place: &str) -> Result<(&'m str, Span), Error> {
    match &meta.value {
        Value::Str(value, span) => Ok((value, *span)),
        _ => Err(unsupported(meta, owner, place)),
    }
}

/// `value` read as Rust tokens (a path or a type), at the literal's span.
fn tokens_of(
    value: &str,
    span: Span,
    meta: &Meta,
    owner: &Owner<'_>,
) -> Result<TokenStream, Error> {
    match TokenStream::from_str(value) {
        Ok(tokens) => Ok(respan(tokens, span)),
        Err(_) => Err(Error::new(
            span,
            format!(
                "`#[serde({} = \"{value}\")]` on {owner} ({}): `{value}` does not lex",
                meta.name(),
                at(span)
            ),
        )),
    }
}

fn respan(tokens: TokenStream, span: Span) -> TokenStream {
    tokens
        .into_iter()
        .map(|mut tree| {
            if let TokenTree::Group(group) = &tree {
                let mut new = Group::new(group.delimiter(), respan(group.stream(), span));
                new.set_span(span);
                tree = TokenTree::Group(new);
            } else {
                tree.set_span(span);
            }
            tree
        })
        .collect()
}

fn container_attrs(attrs: &[Group], owner: &Owner<'_>) -> Result<Container, Error> {
    const PLACE: &str = "the container";
    let mut out = Container::default();
    for meta in serde_metas(attrs, owner)? {
        let flag = |set: &mut bool| -> Result<(), Error> {
            match (&meta.value, *set) {
                (Value::Flag, false) => {
                    *set = true;
                    Ok(())
                }
                (Value::Flag, true) => Err(duplicate(&meta, owner)),
                _ => Err(unsupported(&meta, owner, PLACE)),
            }
        };
        match meta.name().as_str() {
            "deny_unknown_fields" => flag(&mut out.deny_unknown_fields)?,
            "transparent" => flag(&mut out.transparent)?,
            "untagged" => flag(&mut out.untagged)?,
            "rename_all" => {
                let (value, span) = str_value(&meta, owner, PLACE)?;
                let rule = RenameRule::of(value).ok_or_else(|| {
                    Error::new(
                        span,
                        format!(
                            "`#[serde(rename_all = \"{value}\")]` on {owner} ({}): blaze-serde-derive reads \"lowercase\", \"snake_case\", \"kebab-case\" and \"camelCase\"",
                            at(span)
                        ),
                    )
                })?;
                if out.rename_all.replace(rule).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "tag" => {
                let (value, _) = str_value(&meta, owner, PLACE)?;
                if out.tag.replace(value.to_owned()).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "default" => {
                let default = match &meta.value {
                    Value::Flag => DefaultOf::Trait,
                    Value::Str(value, span) => {
                        DefaultOf::Path(tokens_of(value, *span, &meta, owner)?)
                    }
                    Value::List(_) => return Err(unsupported(&meta, owner, PLACE)),
                };
                if out.default.replace(default).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "from" | "try_from" => {
                let (value, span) = str_value(&meta, owner, PLACE)?;
                let ty = tokens_of(value, span, &meta, owner)?;
                let slot = match meta.name().as_str() {
                    "from" => &mut out.from,
                    _ => &mut out.try_from,
                };
                if slot.replace(ty).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "bound" => {
                let Value::List(list) = &meta.value else {
                    return Err(unsupported(&meta, owner, PLACE));
                };
                for side in list {
                    let (value, span) = str_value(side, owner, PLACE)?;
                    let predicates =
                        split_commas(tokens_of(value, span, side, owner)?.into_iter().collect())
                            .into_iter()
                            .map(|it| it.into_iter().collect())
                            .collect();
                    let slot = match side.name().as_str() {
                        "serialize" => &mut out.bound_serialize,
                        "deserialize" => &mut out.bound_deserialize,
                        _ => return Err(unsupported(side, owner, PLACE)),
                    };
                    if slot.replace(predicates).is_some() {
                        return Err(duplicate(side, owner));
                    }
                }
            }
            _ => return Err(unsupported(&meta, owner, PLACE)),
        }
    }
    Ok(out)
}

fn field_attrs(attrs: &[Group], owner: &Owner<'_>, ty: &TokenStream) -> Result<FieldAttrs, Error> {
    const PLACE: &str = "the field";
    let mut out = FieldAttrs::default();
    let mut borrow = None;
    let mut with = None;
    for meta in serde_metas(attrs, owner)? {
        let path = |meta: &Meta| -> Result<TokenStream, Error> {
            let (value, span) = str_value(meta, owner, PLACE)?;
            tokens_of(value, span, meta, owner)
        };
        let set = |slot: &mut Option<TokenStream>, value: TokenStream| -> Result<(), Error> {
            match slot.replace(value) {
                None => Ok(()),
                Some(_) => Err(duplicate(&meta, owner)),
            }
        };
        match meta.name().as_str() {
            "rename" => {
                let (value, _) = str_value(&meta, owner, PLACE)?;
                if out.rename.replace(value.to_owned()).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "default" => {
                let default = match &meta.value {
                    Value::Flag => DefaultOf::Trait,
                    Value::Str(..) => DefaultOf::Path(path(&meta)?),
                    Value::List(_) => return Err(unsupported(&meta, owner, PLACE)),
                };
                if out.default.replace(default).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "skip" | "flatten" => {
                if !matches!(meta.value, Value::Flag) {
                    return Err(unsupported(&meta, owner, PLACE));
                }
                let slot = match meta.name().as_str() {
                    "skip" => &mut out.skip,
                    _ => &mut out.flatten,
                };
                if std::mem::replace(slot, true) {
                    return Err(duplicate(&meta, owner));
                }
            }
            "borrow" => {
                if !matches!(meta.value, Value::Flag) {
                    return Err(unsupported(&meta, owner, PLACE));
                }
                if borrow.replace(meta.span()).is_some() {
                    return Err(duplicate(&meta, owner));
                }
            }
            "skip_serializing_if" => set(&mut out.skip_serializing_if, path(&meta)?)?,
            "serialize_with" => set(&mut out.serialize_with, path(&meta)?)?,
            "deserialize_with" => set(&mut out.deserialize_with, path(&meta)?)?,
            "with" => set(&mut with, path(&meta)?)?,
            _ => return Err(unsupported(&meta, owner, PLACE)),
        }
    }
    if let Some(module) = with {
        if out.serialize_with.is_some() || out.deserialize_with.is_some() {
            return Err(Error::new(
                Span::call_site(),
                format!(
                    "{owner}: `#[serde(with)]` together with `serialize_with` or `deserialize_with`"
                ),
            ));
        }
        out.serialize_with = Some(join(&module, "serialize"));
        out.deserialize_with = Some(join(&module, "deserialize"));
    }
    out.borrowed = match borrow {
        Some(span) => {
            let lifetimes = generics::lifetimes_in(ty);
            if lifetimes.is_empty() {
                return Err(Error::new(
                    span,
                    format!(
                        "`#[serde(borrow)]` on {owner} ({}): its type has no lifetime to borrow",
                        at(span)
                    ),
                ));
            }
            out.borrow_cow = cow_of(ty);
            lifetimes
        }
        None => match generics::implicit_borrow(ty) {
            Some(lifetime) => vec![lifetime],
            None => Vec::new(),
        },
    };
    if out.flatten
        && (out.skip
            || out.rename.is_some()
            || out.default.is_some()
            || out.skip_serializing_if.is_some())
    {
        return Err(Error::new(
            Span::call_site(),
            format!(
                "{owner}: `#[serde(flatten)]` with another attribute blaze-serde-derive does not combine it with"
            ),
        ));
    }
    Ok(out)
}

/// `module::function`.
fn join(module: &TokenStream, function: &str) -> TokenStream {
    let span = module.clone().into_iter().next().map_or_else(Span::call_site, |it| it.span());
    let mut out: Vec<TokenTree> = module.clone().into_iter().collect();
    let mut first = Punct::new(':', Spacing::Joint);
    first.set_span(span);
    let mut second = Punct::new(':', Spacing::Alone);
    second.set_span(span);
    out.push(TokenTree::Punct(first));
    out.push(TokenTree::Punct(second));
    out.push(TokenTree::Ident(Ident::new(function, span)));
    out.into_iter().collect()
}

/// Whether `ty` is `Cow<'a, str>` or `Cow<'a, [u8]>` (by its last path segment, as serde
/// reads it).
fn cow_of(ty: &TokenStream) -> Option<CowOf> {
    let tokens: Vec<TokenTree> = ty.clone().into_iter().collect();
    let at = tokens
        .iter()
        .rposition(|it| matches!(it, TokenTree::Ident(it) if it.to_string() == "Cow"))?;
    let rest = &tokens[at + 1..];
    let is =
        |tree: &TokenTree, ch: char| matches!(tree, TokenTree::Punct(it) if it.as_char() == ch);
    match rest {
        [open, quote, TokenTree::Ident(_), comma, TokenTree::Ident(inner), close]
            if is(open, '<')
                && is(quote, '\'')
                && is(comma, ',')
                && is(close, '>')
                && inner.to_string() == "str" =>
        {
            Some(CowOf::Str)
        }
        [open, quote, TokenTree::Ident(_), comma, TokenTree::Group(inner), close]
            if is(open, '<')
                && is(quote, '\'')
                && is(comma, ',')
                && is(close, '>')
                && inner.delimiter() == Delimiter::Bracket
                && inner.stream().to_string().replace(' ', "") == "u8" =>
        {
            Some(CowOf::Bytes)
        }
        _ => None,
    }
}

fn variant_attrs(attrs: &[Group], owner: &Owner<'_>) -> Result<VariantAttrs, Error> {
    let mut out = VariantAttrs::default();
    for meta in serde_metas(attrs, owner)? {
        match (meta.name().as_str(), &meta.value) {
            ("other", Value::Flag) => {
                if std::mem::replace(&mut out.other, true) {
                    return Err(duplicate(&meta, owner));
                }
            }
            _ => return Err(unsupported(&meta, owner, "the variant")),
        }
    }
    Ok(out)
}

fn named_fields(
    item: &str,
    variant: Option<&str>,
    body: &Group,
    rename_all: Option<RenameRule>,
) -> Result<Fields, Error> {
    let mut list = Vec::new();
    for (index, chunk) in split_commas(body.stream().into_iter().collect()).into_iter().enumerate()
    {
        let mut c = Cursor::new(chunk);
        let attrs = c.outer_attrs();
        c.visibility();
        let ident = c.ident("a field's name")?;
        if !c.peek_punct(':') {
            return Err(Error::new(
                ident.span(),
                format!(
                    "field `{ident}` of `{item}` ({}): no `:` after its name",
                    at(ident.span())
                ),
            ));
        }
        c.next();
        let ty = c.rest();
        let name = unraw(&ident);
        let path = match variant {
            Some(variant) => format!("{variant}::{name}"),
            None => name.clone(),
        };
        let owner = Owner { item, field: Some(&path) };
        let attrs = field_attrs(&attrs, &owner, &ty)?;
        let key = match (&attrs.rename, rename_all) {
            (Some(rename), _) => rename.clone(),
            (None, Some(rule)) => rule.field(&name),
            (None, None) => name,
        };
        list.push(Field { ident: Some(ident), index, key, ty, attrs });
    }
    Ok(Fields { style: Style::Named, list })
}

fn tuple_fields(item: &str, variant: Option<&str>, body: &Group) -> Result<Fields, Error> {
    let mut list = Vec::new();
    for (index, chunk) in split_commas(body.stream().into_iter().collect()).into_iter().enumerate()
    {
        let mut c = Cursor::new(chunk);
        let attrs = c.outer_attrs();
        c.visibility();
        let ty = c.rest();
        let path = match variant {
            Some(variant) => format!("{variant}::{index}"),
            None => index.to_string(),
        };
        let owner = Owner { item, field: Some(&path) };
        let attrs = field_attrs(&attrs, &owner, &ty)?;
        if attrs.rename.is_some() {
            return Err(Error::new(
                body.span(),
                format!("{owner}: `#[serde(rename)]` on a tuple field"),
            ));
        }
        list.push(Field { ident: None, index, key: index.to_string(), ty, attrs });
    }
    Ok(Fields { style: Style::Tuple, list })
}

fn variants(
    item: &str,
    body: &Group,
    rename_all: Option<RenameRule>,
) -> Result<Vec<Variant>, Error> {
    let mut out = Vec::new();
    for chunk in split_commas(body.stream().into_iter().collect()) {
        let mut c = Cursor::new(chunk);
        let attrs = c.outer_attrs();
        let ident = c.ident("a variant's name")?;
        let name = unraw(&ident);
        let fields = match c.peek() {
            Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => {
                let fields = named_fields(item, Some(&name), group, None)?;
                c.next();
                fields
            }
            Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
                let fields = tuple_fields(item, Some(&name), group)?;
                c.next();
                fields
            }
            _ => Fields { style: Style::Unit, list: Vec::new() },
        };
        let owner = Owner { item, field: Some(&name) };
        let attrs = variant_attrs(&attrs, &owner)?;
        let key = match rename_all {
            Some(rule) => rule.variant(&name),
            None => name,
        };
        out.push(Variant { ident, key, fields, attrs });
    }
    Ok(out)
}

/// `tokens` cut at each comma outside `<..>`, the empty pieces dropped.
pub(crate) fn split_commas(tokens: Vec<TokenTree>) -> Vec<Vec<TokenTree>> {
    let mut out = Vec::new();
    let mut piece = Vec::new();
    let mut depth = 0u32;
    let mut arrow = false;
    for tree in tokens {
        let mut is_arrow_head = false;
        if let TokenTree::Punct(punct) = &tree {
            match punct.as_char() {
                '<' => depth += 1,
                '>' if !arrow => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    if !piece.is_empty() {
                        out.push(std::mem::take(&mut piece));
                    }
                    continue;
                }
                '-' | '=' => is_arrow_head = punct.spacing() == Spacing::Joint,
                _ => {}
            }
        }
        arrow = is_arrow_head;
        piece.push(tree);
    }
    if !piece.is_empty() {
        out.push(piece);
    }
    out
}

/// A position in a list of token trees.
pub(crate) struct Cursor {
    tokens: Vec<TokenTree>,
    at: usize,
}

impl Cursor {
    pub(crate) fn new(tokens: Vec<TokenTree>) -> Cursor {
        Cursor { tokens, at: 0 }
    }

    pub(crate) fn peek(&self) -> Option<&TokenTree> {
        self.tokens.get(self.at)
    }

    pub(crate) fn next(&mut self) -> Option<TokenTree> {
        let tree = self.tokens.get(self.at).cloned();
        self.at += 1;
        tree
    }

    pub(crate) fn peek_punct(&self, ch: char) -> bool {
        matches!(self.peek(), Some(TokenTree::Punct(it)) if it.as_char() == ch)
    }

    fn peek_ident(&self, text: &str) -> bool {
        matches!(self.peek(), Some(TokenTree::Ident(it)) if it.to_string() == text)
    }

    pub(crate) fn ident(&mut self, what: &str) -> Result<Ident, Error> {
        match self.next() {
            Some(TokenTree::Ident(ident)) => Ok(ident),
            other => {
                let span = other.as_ref().map_or_else(Span::call_site, TokenTree::span);
                Err(Error::new(span, format!("blaze-serde-derive expected {what} at {}", at(span))))
            }
        }
    }

    /// The bracket groups of the outer attributes here, `#[..]` each.
    fn outer_attrs(&mut self) -> Vec<Group> {
        let mut out = Vec::new();
        while self.peek_punct('#') {
            match self.tokens.get(self.at + 1) {
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket => {
                    out.push(group.clone());
                    self.at += 2;
                }
                _ => break,
            }
        }
        out
    }

    fn visibility(&mut self) {
        if self.peek_ident("pub") {
            self.at += 1;
            if matches!(self.peek(), Some(TokenTree::Group(it)) if it.delimiter() == Delimiter::Parenthesis)
            {
                self.at += 1;
            }
        }
    }

    /// The tokens between `<` (here) and its `>`, not counting the `>` of `->`.
    fn angle(&mut self) -> Result<Vec<TokenTree>, Error> {
        let open = self.next().map_or_else(Span::call_site, |it| it.span());
        let mut depth = 1u32;
        let mut out = Vec::new();
        let mut arrow = false;
        while let Some(tree) = self.next() {
            let mut is_arrow_head = false;
            if let TokenTree::Punct(punct) = &tree {
                match punct.as_char() {
                    '<' => depth += 1,
                    '>' if !arrow => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(out);
                        }
                    }
                    '-' | '=' => is_arrow_head = punct.spacing() == Spacing::Joint,
                    _ => {}
                }
            }
            arrow = is_arrow_head;
            out.push(tree);
        }
        Err(Error::new(
            open,
            format!("blaze-serde-derive found no `>` closing the generics at {}", at(open)),
        ))
    }

    /// The predicates of a `where` here, up to a `{` or `;` outside `<..>`; none when there is
    /// no `where`.
    fn where_clause(&mut self) -> Vec<TokenStream> {
        if !self.peek_ident("where") {
            return Vec::new();
        }
        self.at += 1;
        let mut tokens = Vec::new();
        let mut depth = 0u32;
        while let Some(tree) = self.peek() {
            match tree {
                TokenTree::Group(group) if depth == 0 && group.delimiter() == Delimiter::Brace => {
                    break;
                }
                TokenTree::Punct(punct) if depth == 0 && punct.as_char() == ';' => break,
                TokenTree::Punct(punct) if punct.as_char() == '<' => depth += 1,
                TokenTree::Punct(punct) if punct.as_char() == '>' => {
                    depth = depth.saturating_sub(1)
                }
                _ => {}
            }
            tokens.push(tree.clone());
            self.at += 1;
        }
        split_commas(tokens).into_iter().map(|it| it.into_iter().collect()).collect()
    }

    pub(crate) fn rest(&mut self) -> TokenStream {
        let rest = self.tokens.get(self.at..).unwrap_or_default().iter().cloned().collect();
        self.at = self.tokens.len();
        rest
    }
}
