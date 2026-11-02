//! A name not in the case rustc's style lints ask for: `non_camel_case_types` (an early lint),
//! `non_snake_case` and `non_upper_case_globals` (late lints), with rustc's sort of the name
//! ("function", "trait method", "associated constant", ..), its conversion of the name and its
//! suggestion. hir finds the names by rust-analyzer's rules; each is checked again by rustc's
//! (`is_upper_camel_case`, `is_snake_case`, all upper case), so a name only rust-analyzer
//! would flag gives nothing.
//!
//! Follows rustc's `compiler/rustc_lint/src/nonstandard_style.rs` and `lints.rs`
//! (`NonCamelCaseType`, `NonSnakeCaseDiag`, `NonUpperCaseGlobal`), and `lib.rs` for which
//! pass each runs in (MIT or Apache-2.0).

use std::fmt::{self, Write as _};

use hir::{HasVisibility as _, InFile, Visibility};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::registry::LintId,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// Which of rustc's lints checks a name of hir's kind.
#[derive(Clone, Copy)]
enum Rule {
    Camel,
    Snake,
    Upper,
    Unknown,
}

/// hir's kind of a name as its `Display` writes it, on the stack: hir does not export the kind.
struct Kind {
    bytes: [u8; 16],
    len: usize,
}

impl fmt::Write for Kind {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn rule(d: &hir::IncorrectCase) -> Rule {
    let mut kind = Kind { bytes: [0; 16], len: 0 };
    if write!(kind, "{}", d.ident_type).is_err() {
        return Rule::Unknown;
    }
    match std::str::from_utf8(&kind.bytes[..kind.len]) {
        Ok("Structure" | "Enum" | "Union" | "Trait" | "Type alias" | "Variant") => Rule::Camel,
        Ok("Function" | "Variable" | "Parameter" | "Field" | "Module") => Rule::Snake,
        Ok("Constant" | "Static variable") => Rule::Upper,
        _ => Rule::Unknown,
    }
}

/// A name in rustc's case is one check of its text: the tree is read only for a name rustc
/// flags.
pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::IncorrectCase) -> Vec<Found> {
    let raw = d.ident_text.as_str();
    let name = raw.strip_prefix("r#").unwrap_or(raw);
    let rule = rule(d);
    const CAMEL: LintId = LintId::named("non_camel_case_types");
    const SNAKE: LintId = LintId::named("non_snake_case");
    const UPPER: LintId = LintId::named("non_upper_case_globals");
    let in_case = match rule {
        Rule::Camel => !cx.lint_may_fire(CAMEL) || is_upper_camel_case(name),
        Rule::Snake => !cx.lint_may_fire(SNAKE) || is_snake_case(name),
        Rule::Upper => !cx.lint_may_fire(UPPER) || name.chars().all(|c| c.to_uppercase().eq([c])),
        Rule::Unknown => false,
    };
    if in_case {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let name_ptr = InFile::new(d.file, d.ident);
    let name_node = cx.node_view(name_ptr);
    // alloc: empty, allocates nothing
    let Some(owner) = name_node.value.syntax().parent() else { return Vec::new() };
    let at = |node: ast::Node<'_>| InFile::new(d.file, ast::ptr(node));
    let found = match rule {
        Rule::Camel => {
            // rustc's early pass has no `check_foreign_item` for this lint.
            let foreign = owner.kind() == SyntaxKind::TYPE_ALIAS
                && owner.parent().is_some_and(|it| it.kind() == SyntaxKind::EXTERN_ITEM_LIST);
            if foreign {
                // alloc: empty, allocates nothing
                return Vec::new();
            }
            let span = cx.span_of_ptr(name_ptr);
            let (sort, lint_at) = camel_sort(owner);
            let mut diag =
                // alloc: the error's text, for an error hir found
                Diag::lint("non_camel_case_types", format!("{sort} `{name}` should have an upper camel case name"), span.clone());
            let cc = to_upper_camel_case(name);
            match cc != name {
                true => diag.suggestion(span, "convert the identifier to upper camel case", cc, Applicability::MaybeIncorrect),
                false => diag.span_label(span, "should have an UpperCamelCase name"),
            };
            Found::lint(diag, Phase::Resolve, at(lint_at))
        }
        Rule::Snake => {
            let span = cx.span_of_ptr(name_ptr);
            let sort = snake_sort(owner);
            // alloc: the error's text, for an error hir found
            let mut diag = Diag::lint("non_snake_case", format!("{sort} `{name}` should have a snake case name"), span.clone());
            let sc = to_snake_case(name);
            if sc == name {
                diag.span_label(span, "should have a snake_case name");
            } else if !is_reserved(&sc, cx.edition) {
                diag.suggestion(span, "convert the identifier to snake case", sc, Applicability::MaybeIncorrect);
            } else if can_be_raw(&sc) {
                diag.suggestion(
                    span,
                    "rename the identifier or convert it to a snake case raw identifier",
                    // alloc: the error's text, for an error hir found
                    format!("r#{sc}"),
                    Applicability::MaybeIncorrect,
                );
            } else {
                // alloc: the error's text, for an error hir found
                diag.note(format!("`{sc}` cannot be used as a raw identifier"));
                diag.suggestion(span, "rename the identifier", "", Applicability::MaybeIncorrect);
            }
            Found::lint(diag, Phase::Late, at(owner))
        }
        Rule::Upper => {
            let span = cx.span_of_ptr(name_ptr);
            let (sort, exported) = upper_sort(cx, owner);
            let mut diag =
                // alloc: the error's text, for an error hir found
                Diag::lint("non_upper_case_globals", format!("{sort} `{name}` should have an upper case name"), span.clone());
            let uc = to_snake_case(name).to_uppercase();
            match uc != name {
                true => {
                    let applicability = match exported {
                        Some(false) => Applicability::MachineApplicable,
                        Some(true) | None => Applicability::MaybeIncorrect,
                    };
                    diag.suggestion(span, "convert the identifier to upper case", uc, applicability)
                }
                false => diag.span_label(span, "should have an UPPER_CASE name"),
            };
            Found::lint(diag, Phase::Late, at(owner))
        }
        Rule::Unknown => {
            let diag = Diag::error(
                None,
                // alloc: the error's text, for an error hir found
                format!("blaze does not know rustc's style lint of a name of kind `{}`: `{name}`", d.ident_type),
                cx.span_of_ptr(name_ptr),
            );
            Found::new(diag, Phase::Late)
        }
    };
    // alloc: the error's parts, for an error hir found
    vec![found]
}

/// The container of an associated item: the impl or trait its item list is in.
fn assoc_container(item: ast::Node<'_>) -> Option<ast::Node<'_>> {
    let list = item.parent().filter(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)?;
    list.parent()
}

/// rustc's sort of a type-like name, and the node its lint is emitted at: an associated type
/// of an inherent impl is linted from the impl's `check_item`, so the impl's attributes
/// decide its level.
fn camel_sort(owner: ast::Node<'_>) -> (&'static str, ast::Node<'_>) {
    match owner.kind() {
        SyntaxKind::TRAIT => match ast::Trait::cast(owner).and_then(|it| it.eq_token()) {
            Some(_) => ("trait alias", owner),
            None => ("trait", owner),
        },
        SyntaxKind::VARIANT => ("variant", owner),
        SyntaxKind::TYPE_ALIAS => match assoc_container(owner) {
            Some(container) if container.kind() == SyntaxKind::IMPL => ("associated type", container),
            Some(_) => ("associated type", owner),
            None => ("type", owner),
        },
        _ => ("type", owner),
    }
}

fn snake_sort(owner: ast::Node<'_>) -> &'static str {
    match owner.kind() {
        SyntaxKind::FN => match assoc_container(owner).map(|it| it.kind()) {
            Some(SyntaxKind::IMPL) => "method",
            Some(SyntaxKind::TRAIT) => "trait method",
            _ => "function",
        },
        SyntaxKind::MODULE => "module",
        SyntaxKind::RECORD_FIELD => "structure field",
        _ => "variable",
    }
}

/// rustc's sort of a global, and whether the item is exported (rustc suggests renaming its uses
/// only when it is not), when rustc asks: never for an associated constant.
fn upper_sort<'db>(cx: &Cx<'_, 'db>, owner: ast::Node<'db>) -> (&'static str, Option<bool>) {
    if owner.kind() == SyntaxKind::CONST && assoc_container(owner).is_some() {
        return ("associated constant", None);
    }
    if let Some(it) = ast::Const::cast(owner) {
        let def = cx.vsema.to_def(it);
        return ("constant", def.map(|it| exported(cx, it.visibility(cx.db), it.module(cx.db))));
    }
    if let Some(it) = ast::Static::cast(owner) {
        let def = cx.vsema.to_def(it);
        return ("static variable", def.map(|it| exported(cx, it.visibility(cx.db), it.module(cx.db))));
    }
    ("constant", None)
}

/// Whether an item of `module` with `visibility` is reachable from outside its crate by its
/// path: public, in public modules up to the crate root.
fn exported(cx: &Cx<'_, '_>, visibility: Visibility, module: hir::Module) -> bool {
    visibility == Visibility::Public
        && std::iter::successors(Some(module), |it| it.parent(cx.db))
            .filter(|it| it.parent(cx.db).is_some())
            .all(|it| it.visibility(cx.db) == Visibility::Public)
}

/// rustc's `Ident::is_reserved` in `edition`: a keyword, a reserved one, or `_`.
fn is_reserved(name: &str, edition: span::Edition) -> bool {
    name.is_empty() || name == "_" || SyntaxKind::from_keyword(name, edition).is_some()
}

/// rustc's `Symbol::can_be_raw`.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "self" | "Self" | "super" | "crate")
}

/// `char::to_titlecase`, which rustc's lint uses and std has only unstably: Unicode's title
/// case mapping (`UnicodeData.txt` and `SpecialCasing.txt`), which is the upper case one but
/// for the characters below.
fn to_titlecase(c: char) -> TitleCase {
    let special: &'static [char] = match c {
        'ß' => &['S', 's'],
        '\u{FB00}' => &['F', 'f'],
        '\u{FB01}' => &['F', 'i'],
        '\u{FB02}' => &['F', 'l'],
        '\u{FB03}' => &['F', 'f', 'i'],
        '\u{FB04}' => &['F', 'f', 'l'],
        '\u{FB05}' | '\u{FB06}' => &['S', 't'],
        '\u{0587}' => &['\u{0535}', '\u{0582}'],
        '\u{FB13}' => &['\u{0544}', '\u{0576}'],
        '\u{FB14}' => &['\u{0544}', '\u{0565}'],
        '\u{FB15}' => &['\u{0544}', '\u{056B}'],
        '\u{FB16}' => &['\u{054E}', '\u{0576}'],
        '\u{FB17}' => &['\u{0544}', '\u{056D}'],
        '\u{1FB2}' => &['\u{1FBA}', '\u{0345}'],
        '\u{1FB4}' => &['\u{0386}', '\u{0345}'],
        '\u{1FC2}' => &['\u{1FCA}', '\u{0345}'],
        '\u{1FC4}' => &['\u{0389}', '\u{0345}'],
        '\u{1FF2}' => &['\u{1FFA}', '\u{0345}'],
        '\u{1FF4}' => &['\u{038F}', '\u{0345}'],
        '\u{1FB7}' => &['\u{0391}', '\u{0342}', '\u{0345}'],
        '\u{1FC7}' => &['\u{0397}', '\u{0342}', '\u{0345}'],
        '\u{1FF7}' => &['\u{03A9}', '\u{0342}', '\u{0345}'],
        _ => &[],
    };
    if !special.is_empty() {
        return TitleCase::Special(special.iter().copied());
    }
    let offset = |by: u32| char::from_u32(c as u32 + by).expect("a letter of the Greek Extended block");
    let single = match c {
        '\u{01C4}'..='\u{01C6}' => '\u{01C5}',
        '\u{01C7}'..='\u{01C9}' => '\u{01C8}',
        '\u{01CA}'..='\u{01CC}' => '\u{01CB}',
        '\u{01F1}'..='\u{01F3}' => '\u{01F2}',
        '\u{1F80}'..='\u{1F87}' | '\u{1F90}'..='\u{1F97}' | '\u{1FA0}'..='\u{1FA7}' => offset(8),
        '\u{1FB3}' | '\u{1FC3}' | '\u{1FF3}' => offset(9),
        '\u{10D0}'..='\u{10FA}' | '\u{10FD}'..='\u{10FF}' => c,
        _ if is_titlecase(c) => c,
        _ => return TitleCase::Upper(c.to_uppercase()),
    };
    TitleCase::One(std::iter::once(single))
}

/// The characters of a title case mapping, without a buffer.
enum TitleCase {
    Special(std::iter::Copied<std::slice::Iter<'static, char>>),
    One(std::iter::Once<char>),
    Upper(std::char::ToUppercase),
}

impl Iterator for TitleCase {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self {
            TitleCase::Special(it) => it.next(),
            TitleCase::One(it) => it.next(),
            TitleCase::Upper(it) => it.next(),
        }
    }
}

/// `char::is_titlecase`: the letters of Unicode's category `Lt`.
fn is_titlecase(c: char) -> bool {
    matches!(
        c,
        '\u{01C5}'
            | '\u{01C8}'
            | '\u{01CB}'
            | '\u{01F2}'
            | '\u{1F88}'..='\u{1F8F}'
            | '\u{1F98}'..='\u{1F9F}'
            | '\u{1FA8}'..='\u{1FAF}'
            | '\u{1FBC}'
            | '\u{1FCC}'
            | '\u{1FFC}'
    )
}

/// A character that has case: one rustc can change the case of.
fn char_has_case(c: char) -> bool {
    !c.to_lowercase().eq(to_titlecase(c))
}

fn changes_when_titlecased(c: char) -> bool {
    !to_titlecase(c).eq([c])
}

fn not_camel_case(s: &str) -> bool {
    let mut last = '\0';
    s.chars().any(|snd| {
        let fst = std::mem::replace(&mut last, snd);
        match (fst, snd) {
            ('_', '_') => true,
            ('_', _) if char_has_case(snd) => true,
            (_, '_') if char_has_case(fst) => true,
            _ => snd.is_uppercase() && changes_when_titlecased(snd),
        }
    })
}

pub(crate) fn is_upper_camel_case(name: &str) -> bool {
    let name = name.trim_matches('_');
    let Some(first) = name.chars().next() else { return true };
    !(changes_when_titlecased(first) || not_camel_case(name))
}

pub(crate) fn to_upper_camel_case(s: &str) -> String {
    s.trim_matches('_')
        .split('_')
        .filter(|component| !component.is_empty())
        .map(|component| {
            // alloc: the suggestion's text, built only for a name not in upper camel case
            let mut camel_cased_component = String::new();
            let mut new_word = true;
            let mut prev_is_lower_case = true;
            let mut prev_is_lowercased_sigma = false;
            for c in component.chars() {
                if prev_is_lower_case && (c.is_uppercase() || is_titlecase(c)) {
                    new_word = true;
                }
                if new_word {
                    camel_cased_component.extend(to_titlecase(c));
                } else {
                    camel_cased_component.extend(c.to_lowercase());
                }
                prev_is_lower_case = c.is_lowercase() || is_titlecase(c);
                prev_is_lowercased_sigma = !new_word && c == 'Σ';
                new_word = false;
            }
            if prev_is_lowercased_sigma {
                camel_cased_component.pop();
                camel_cased_component.push('ς');
            }
            camel_cased_component
        })
        // alloc: the suggestion's text, built only for a name not in upper camel case
        .fold((String::new(), None), |(acc, prev): (String, Option<String>), next| {
            let join = match prev {
                Some(prev) => {
                    let l = prev.chars().last().expect("a component is not empty");
                    let f = next.chars().next().expect("a component is not empty");
                    !char_has_case(l) && !char_has_case(f)
                }
                None => false,
            };
            (acc + if join { "_" } else { "" } + &next, Some(next))
        })
        .0
}

fn is_snake_case(ident: &str) -> bool {
    if ident.is_empty() {
        return true;
    }
    let ident = ident.trim_start_matches('\'');
    let ident = ident.trim_matches('_');
    if ident.contains("__") {
        return false;
    }
    ident.chars().all(|c| c.to_lowercase().eq([c]))
}

fn to_snake_case(mut name: &str) -> String {
    // alloc: the suggestion's words, built only for a name hir flagged and rustc flags
    let mut words = vec![];
    name = name.trim_start_matches(|c: char| {
        if c == '_' {
            // alloc: empty, allocates nothing
            words.push(String::new());
            true
        } else {
            false
        }
    });
    for s in name.split('_') {
        let mut last_upper = false;
        // alloc: the suggestion's words, built only for a name hir flagged and rustc flags
        let mut buf = String::new();
        if s.is_empty() {
            continue;
        }
        for ch in s.chars() {
            let upper = ch.is_uppercase() || is_titlecase(ch);
            if !buf.is_empty() && buf != "'" && upper && !last_upper {
                words.push(buf.to_lowercase());
                // alloc: empty, allocates nothing
                buf = String::new();
            }
            last_upper = upper;
            buf.push(ch);
        }
        words.push(buf.to_lowercase());
    }
    words.join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_convert_as_rustc_converts_them() {
        assert_eq!(to_snake_case("fooBar"), "foo_bar");
        assert_eq!(to_snake_case("FOO_BAR"), "foo_bar");
        assert_eq!(to_snake_case("__Foo"), "__foo");
        assert_eq!(to_snake_case("X"), "x");
        assert_eq!(to_snake_case("ABCd"), "abcd");
        assert_eq!(to_upper_camel_case("foo_bar"), "FooBar");
        assert_eq!(to_upper_camel_case("__foo__"), "Foo");
        assert_eq!(to_upper_camel_case("x86_64"), "X86_64");
        assert_eq!(to_upper_camel_case("camelCase"), "CamelCase");
        assert_eq!(to_snake_case("foo_bar").to_uppercase(), "FOO_BAR");
    }

    #[test]
    fn names_are_checked_as_rustc_checks_them() {
        assert!(is_upper_camel_case("X86_64"));
        assert!(is_upper_camel_case("__Foo__"));
        assert!(!is_upper_camel_case("foo"));
        assert!(!is_upper_camel_case("Foo__Bar"));
        assert!(!is_upper_camel_case("Foo_bar"));
        assert!(is_snake_case("_foo_"));
        assert!(!is_snake_case("foo__bar"));
        assert!(!is_snake_case("fooBar"));
        assert!(is_snake_case("'a"));
    }

    #[test]
    fn a_reserved_conversion_is_suggested_raw_or_not_at_all() {
        assert!(is_reserved("async", span::Edition::Edition2018));
        assert!(!is_reserved("async", span::Edition::Edition2015));
        assert!(can_be_raw("async"));
        assert!(!can_be_raw("self"));
    }
}
