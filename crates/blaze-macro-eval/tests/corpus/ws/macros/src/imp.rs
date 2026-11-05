//! The macros, against `crate::proc_macro` alone; `lib.rs` declares them.

use crate::proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

fn ident(name: &str) -> TokenTree {
    TokenTree::Ident(Ident::new(name, Span::call_site()))
}

fn punct(ch: char, spacing: Spacing) -> TokenTree {
    TokenTree::Punct(Punct::new(ch, spacing))
}

fn group(delimiter: Delimiter, stream: TokenStream) -> TokenTree {
    TokenTree::Group(Group::new(delimiter, stream))
}

fn is_punct(tree: &TokenTree, ch: char) -> bool {
    matches!(tree, TokenTree::Punct(it) if it.as_char() == ch)
}

fn is_ident(tree: &TokenTree, name: &str) -> bool {
    matches!(tree, TokenTree::Ident(it) if it.to_string() == name)
}

fn static_lifetime(out: &mut Vec<TokenTree>) {
    out.push(punct('\'', Spacing::Joint));
    out.push(ident("static"));
}

/// `pub const NAME: &'static str = "text";`
fn str_const(name: Ident, text: &str, out: &mut Vec<TokenTree>) {
    out.extend([ident("pub"), ident("const"), TokenTree::Ident(name), punct(':', Spacing::Alone), punct('&', Spacing::Alone)]);
    static_lifetime(out);
    out.extend([
        ident("str"),
        punct('=', Spacing::Alone),
        TokenTree::Literal(Literal::string(text)),
        punct(';', Spacing::Alone),
    ]);
}

fn one_ident(input: TokenStream, what: &str) -> Ident {
    let mut trees = input.into_iter();
    match (trees.next(), trees.next()) {
        (Some(TokenTree::Ident(name)), None) => name,
        _ => panic!("{what}: expected one name"),
    }
}

/// `make_fn!(name: Type = value)` is `pub fn name() -> Type { value }`.
pub(crate) fn make_fn(input: TokenStream) -> TokenStream {
    let mut trees = input.into_iter();
    let name = match trees.next() {
        Some(TokenTree::Ident(name)) => name,
        _ => panic!("make_fn!: expected a name"),
    };
    match trees.next() {
        Some(colon) if is_punct(&colon, ':') => {}
        _ => panic!("make_fn!: expected `:` after the name"),
    }
    let (mut ty, mut value, mut equals) = (Vec::new(), Vec::new(), false);
    for tree in trees {
        if !equals && is_punct(&tree, '=') {
            equals = true;
        } else if equals {
            value.push(tree);
        } else {
            ty.push(tree);
        }
    }
    assert!(equals, "make_fn!: expected `= value` after the type");
    let mut out = vec![
        ident("pub"),
        ident("fn"),
        TokenTree::Ident(name),
        group(Delimiter::Parenthesis, TokenStream::new()),
        punct('-', Spacing::Joint),
        punct('>', Spacing::Alone),
    ];
    out.extend(ty);
    out.push(group(Delimiter::Brace, value.into_iter().collect()));
    out.into_iter().collect()
}

/// Whether an attribute's tokens (inside `#[..]`) are `describe(skip)`.
fn is_skip(attr: &Group) -> bool {
    let trees: Vec<TokenTree> = attr.stream().into_iter().collect();
    match trees.as_slice() {
        [name, TokenTree::Group(args)] if is_ident(name, "describe") => {
            let args: Vec<TokenTree> = args.stream().into_iter().collect();
            matches!(args.as_slice(), [skip] if is_ident(skip, "skip"))
        }
        _ => false,
    }
}

/// `#[derive(Describe)]` on a struct with named fields gives it `FIELDS`, the names of the
/// fields not marked `#[describe(skip)]`, in order.
pub(crate) fn describe(input: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = input.into_iter().collect();
    let at = trees.iter().position(|it| is_ident(it, "struct")).expect("Describe: only on a struct");
    let name = match trees.get(at + 1) {
        Some(TokenTree::Ident(name)) => name.clone(),
        _ => panic!("Describe: the struct has no name"),
    };
    let body = trees[at + 2..]
        .iter()
        .find_map(|it| match it {
            TokenTree::Group(body) if body.delimiter() == Delimiter::Brace => Some(body.stream()),
            _ => None,
        })
        .expect("Describe: only on a struct with named fields");
    let body: Vec<TokenTree> = body.into_iter().collect();
    let (mut fields, mut skip, mut expect_name, mut depth) = (Vec::new(), false, true, 0u32);
    let mut i = 0;
    while i < body.len() {
        let tree = &body[i];
        match tree {
            TokenTree::Punct(it) if it.as_char() == '#' => {
                if let Some(TokenTree::Group(attr)) = body.get(i + 1) {
                    skip |= is_skip(attr);
                    i += 2;
                    continue;
                }
            }
            TokenTree::Punct(it) if it.as_char() == '<' => depth += 1,
            TokenTree::Punct(it) if it.as_char() == '>' && depth > 0 && !(i > 0 && is_punct(&body[i - 1], '-')) => {
                depth -= 1
            }
            TokenTree::Punct(it) if it.as_char() == ',' && depth == 0 => {
                expect_name = true;
                skip = false;
            }
            TokenTree::Punct(it) if it.as_char() == ':' && depth == 0 && expect_name => {
                if let Some(TokenTree::Ident(field)) = i.checked_sub(1).and_then(|it| body.get(it)) {
                    if !skip {
                        fields.push(field.to_string());
                    }
                    expect_name = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let mut list = Vec::new();
    for (at, field) in fields.iter().enumerate() {
        if at > 0 {
            list.push(punct(',', Spacing::Alone));
        }
        list.push(TokenTree::Literal(Literal::string(field)));
    }
    let mut element = vec![punct('&', Spacing::Alone)];
    static_lifetime(&mut element);
    element.push(ident("str"));
    let mut item = vec![ident("pub"), ident("const"), ident("FIELDS"), punct(':', Spacing::Alone), punct('&', Spacing::Alone)];
    static_lifetime(&mut item);
    item.extend([
        group(Delimiter::Bracket, element.into_iter().collect()),
        punct('=', Spacing::Alone),
        punct('&', Spacing::Alone),
        group(Delimiter::Bracket, list.into_iter().collect()),
        punct(';', Spacing::Alone),
    ]);
    [ident("impl"), TokenTree::Ident(name), group(Delimiter::Brace, item.into_iter().collect())].into_iter().collect()
}

/// `#[label("text")] fn f() { .. }` is the function with `let _label: &str = "text";` first in
/// its body, the body keeping its braces' span.
pub(crate) fn label(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut attr = attr.into_iter();
    let text = match (attr.next(), attr.next()) {
        (Some(TokenTree::Literal(text)), None) => text,
        _ => panic!("label: expected one string"),
    };
    let mut trees: Vec<TokenTree> = item.into_iter().collect();
    let body = trees
        .iter_mut()
        .rev()
        .find_map(|it| match it {
            TokenTree::Group(body) if body.delimiter() == Delimiter::Brace => Some(body),
            _ => None,
        })
        .expect("label: expected a function with a body");
    let mut statements = vec![
        ident("let"),
        ident("_label"),
        punct(':', Spacing::Alone),
        punct('&', Spacing::Alone),
        ident("str"),
        punct('=', Spacing::Alone),
        TokenTree::Literal(text),
        punct(';', Spacing::Alone),
    ];
    statements.extend(body.stream());
    let mut labelled = Group::new(Delimiter::Brace, statements.into_iter().collect());
    labelled.set_span(body.span());
    *body = labelled;
    trees.into_iter().collect()
}

/// `boom!(..)` panics, saying how many token trees it was given.
pub(crate) fn boom(input: TokenStream) -> TokenStream {
    panic!("boom: {} tokens", input.into_iter().count())
}

/// `include_data!(NAME, "file")` is `pub const NAME: &'static str = "..";`, the text of the
/// file of the calling package's directory, as `include_str!` would read it.
pub(crate) fn include_data(input: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = input.into_iter().collect();
    let (name, file) = match trees.as_slice() {
        [TokenTree::Ident(name), comma, TokenTree::Literal(file)] if is_punct(comma, ',') => (name.clone(), file.to_string()),
        _ => panic!("include_data!: expected `NAME, \"file\"`"),
    };
    let file = file
        .strip_prefix('"')
        .and_then(|it| it.strip_suffix('"'))
        .expect("include_data!: the file is a plain string literal");
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("include_data!: cargo sets CARGO_MANIFEST_DIR");
    let path = format!("{dir}/{file}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("include_data!: {path}: {error}"));
    let mut out = Vec::new();
    str_const(name, &text, &mut out);
    out.into_iter().collect()
}

/// `env_name!(NAME)` is `NAME`, the calling package's name, and `NAME_UNSET`, whether
/// `CORPUS_UNSET` is unset.
pub(crate) fn env_name(input: TokenStream) -> TokenStream {
    let name = one_ident(input, "env_name!");
    let flag = Ident::new(&format!("{name}_UNSET"), name.span());
    let package = std::env::var("CARGO_PKG_NAME").expect("env_name!: cargo sets CARGO_PKG_NAME");
    let unset = std::env::var_os("CORPUS_UNSET").is_none();
    let mut out = Vec::new();
    str_const(name, &package, &mut out);
    out.extend([
        ident("pub"),
        ident("const"),
        TokenTree::Ident(flag),
        punct(':', Spacing::Alone),
        ident("bool"),
        punct('=', Spacing::Alone),
        ident(if unset { "true" } else { "false" }),
        punct(';', Spacing::Alone),
    ]);
    out.into_iter().collect()
}
