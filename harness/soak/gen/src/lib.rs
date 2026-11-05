//! The macros of the soak workspace, written against `proc_macro` alone so that the
//! workspace needs nothing from a registry.
use proc_macro::{TokenStream, TokenTree};

/// What every expansion is salted with. The soak edits it.
const SALT: u64 = 7;

/// The name of the type a derive is on: the identifier after `struct` or `enum`.
fn type_name(input: TokenStream) -> String {
    let mut tokens = input.into_iter();
    while let Some(token) = tokens.next() {
        if let TokenTree::Ident(word) = &token {
            if matches!(word.to_string().as_str(), "struct" | "enum") {
                return tokens.next().map(|it| it.to_string()).unwrap_or_default();
            }
        }
    }
    String::new()
}

/// `#[derive(Describe)]`: `describe()` is the name of the type and the salt.
#[proc_macro_derive(Describe)]
pub fn describe(input: TokenStream) -> TokenStream {
    let name = type_name(input);
    let code = format!(
        "impl {name} {{ pub fn describe() -> String {{ format!(\"{name}/{{}}\", {SALT}u64) }} }}"
    );
    code.parse().unwrap_or_default()
}

/// `table!(a, b, c)`: an array of each name with its length and the salt.
#[proc_macro]
pub fn table(input: TokenStream) -> TokenStream {
    let names = input.into_iter().filter_map(|token| match token {
        TokenTree::Ident(word) => Some(word.to_string()),
        _ => None,
    });
    let rows: Vec<String> =
        names.map(|it| format!("(\"{it}\", {}u64)", it.len() as u64 + SALT)).collect();
    format!("[{}]", rows.join(", ")).parse().unwrap_or_default()
}

/// `#[twice]`: the function's result, doubled, as `<name>_twice`.
#[proc_macro_attribute]
pub fn twice(_args: TokenStream, item: TokenStream) -> TokenStream {
    let text = item.to_string();
    let name = text.split("fn ").nth(1).and_then(|it| it.split('(').next()).unwrap_or("").trim();
    let more = format!("pub fn {name}_twice() -> u64 {{ {name}().wrapping_mul(2) }}");
    format!("{text}\n{more}").parse().unwrap_or_default()
}
