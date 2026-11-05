//! `#[doc(cfg(..))]` in words, as rustdoc's "Available on ... only" note says it (adapted from
//! `clean/cfg.rs` in librustdoc at rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0).

use syntax::{
    NodeOrToken, SyntaxKind, T,
    view::{self as ast, AstNode, AstToken},
};

enum Cfg {
    Name(String),
    KeyValue(String, String),
    All(Vec<Cfg>),
    Any(Vec<Cfg>),
    Not(Box<Cfg>),
}

/// The condition inside `cfg(..)`, for "Available on **{}** only." The text is Markdown-free
/// HTML: feature names in `<code>`.
pub(crate) fn describe(tt: ast::TokenTree<'_>) -> String {
    let list = parse_list(tt);
    match list.len() {
        1 => render(&list[0], false),
        _ => render(&Cfg::All(list), false),
    }
}

fn parse_list(tt: ast::TokenTree<'_>) -> Vec<Cfg> {
    let mut items: Vec<ast::Element<'_>> =
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    // Drop the delimiters.
    if items.first().is_some_and(|it| matches!(it.kind(), T!['('] | T!['['] | T!['{'])) {
        items.remove(0);
    }
    if items.last().is_some_and(|it| matches!(it.kind(), T![')'] | T![']'] | T!['}'])) {
        items.pop();
    }
    let mut out = vec![];
    let mut at = 0;
    while at < items.len() {
        let NodeOrToken::Token(token) = &items[at] else {
            at += 1;
            continue;
        };
        if token.kind() != SyntaxKind::IDENT {
            at += 1;
            continue;
        }
        let name = token.text().to_owned();
        match items.get(at + 1) {
            Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
                let value = items
                    .get(at + 2)
                    .and_then(|it| it.into_token())
                    .and_then(ast::String::cast)
                    .and_then(|it| it.value().ok().map(|it| it.into_owned()))
                    .unwrap_or_default();
                out.push(Cfg::KeyValue(name, value));
                at += 3;
            }
            Some(NodeOrToken::Node(node)) => {
                let inner = ast::TokenTree::cast(*node).map(parse_list);
                let inner = inner.unwrap_or_default();
                out.push(match name.as_str() {
                    "all" => Cfg::All(inner),
                    "any" => Cfg::Any(inner),
                    "not" => Cfg::Not(Box::new(match inner.len() {
                        1 => inner.into_iter().next().unwrap(),
                        _ => Cfg::All(inner),
                    })),
                    _ => Cfg::Name(name),
                });
                at += 2;
            }
            _ => {
                out.push(Cfg::Name(name));
                at += 1;
            }
        }
    }
    out
}

fn render(cfg: &Cfg, nested: bool) -> String {
    match cfg {
        Cfg::Name(name) => {
            name_word(name).map_or_else(|| format!("<code>{name}</code>"), str::to_owned)
        }
        Cfg::KeyValue(key, value) => key_value_words(key, value),
        Cfg::All(list) => join(list, " and ", nested),
        Cfg::Any(list) => join(list, " or ", nested),
        Cfg::Not(inner) => match &**inner {
            Cfg::KeyValue(key, value) if key == "feature" => {
                format!("crate feature <code>{value}</code> disabled")
            }
            other => format!("non-{}", render(other, true)),
        },
    }
}

fn join(list: &[Cfg], separator: &str, nested: bool) -> String {
    // "crate features `a` and `b`" reads better than repeating the words.
    let features: Option<Vec<String>> = list
        .iter()
        .map(|it| match it {
            Cfg::KeyValue(key, value) if key == "feature" => Some(format!("<code>{value}</code>")),
            _ => None,
        })
        .collect();
    if let Some(features) = features
        && features.len() > 1
    {
        return format!("crate features {}", features.join(separator));
    }
    let parts: Vec<String> = list.iter().map(|it| render(it, true)).collect();
    let joined = parts.join(separator);
    if nested && list.len() > 1 { format!("({joined})") } else { joined }
}

fn name_word(name: &str) -> Option<&'static str> {
    Some(match name {
        "unix" => "Unix",
        "windows" => "Windows",
        "debug_assertions" => "debug-assertions enabled",
        "test" => "test",
        "doc" => "documentation",
        "miri" => "Miri",
        _ => return None,
    })
}

fn key_value_words(key: &str, value: &str) -> String {
    let word = match (key, value) {
        ("feature", feature) => return format!("crate feature <code>{feature}</code>"),
        ("target_os", "linux") => "Linux",
        ("target_os", "macos") => "macOS",
        ("target_os", "windows") => "Windows",
        ("target_os", "ios") => "iOS",
        ("target_os", "android") => "Android",
        ("target_os", "freebsd") => "FreeBSD",
        ("target_os", "netbsd") => "NetBSD",
        ("target_os", "openbsd") => "OpenBSD",
        ("target_os", "dragonfly") => "DragonFly BSD",
        ("target_os", "illumos") => "illumos",
        ("target_os", "solaris") => "Solaris",
        ("target_os", "wasi") => "WASI",
        ("target_os", "redox") => "Redox",
        ("target_os", "fuchsia") => "Fuchsia",
        ("target_family", "unix") => "Unix",
        ("target_family", "windows") => "Windows",
        ("target_family", "wasm") => "WebAssembly",
        ("target_arch", "x86") => "x86",
        ("target_arch", "x86_64") => "x86-64",
        ("target_arch", "aarch64") => "AArch64",
        ("target_arch", "arm") => "ARM",
        ("target_arch", "wasm32") => "WebAssembly",
        ("target_arch", "riscv64") => "RISC-V RV64",
        ("target_env", "gnu") => "GNU",
        ("target_env", "musl") => "musl",
        ("target_env", "msvc") => "MSVC",
        ("target_pointer_width", width) => return format!("{width}-bit"),
        ("target_endian", endian) => return format!("{endian}-endian"),
        ("target_has_atomic", bits) => return format!("target with {bits}-bit atomics"),
        ("target_feature", feature) => return format!("target feature <code>{feature}</code>"),
        _ => return format!("<code>{key}=\"{value}\"</code>"),
    };
    word.to_owned()
}
