//! The name an `extern crate` item written in a block binds, read from its text.
//!
//! The Reference, "Extern crate declarations" [items.extern-crate.intro]: "An extern crate
//! declaration specifies a dependency on an external crate. The external crate is then bound
//! into the declaring scope as the given identifier in the type namespace." Its grammar:
//! `ExternCrate : extern crate CrateRef AsClause? ;`, `CrateRef : IDENTIFIER | self`,
//! `AsClause : as ( IDENTIFIER | _ )`. [items.extern-crate.self]: "If the crate name is
//! `self`, then the current crate is bound ... `extern crate self as foo;`": `self` without
//! `as` is an error ([items.extern-crate.self] "it must be given a name"). [items.extern-crate.
//! underscore.intro]: "An external crate dependency can be declared without binding its name
//! in scope by using an underscore with the form `extern crate foo as _`."
//!
//! The body's lowering keeps a block item's name only from its own `NAME` node, which an
//! `extern crate` has only inside its `as` clause; so the binding is read here, from the
//! item's text (outer attributes and a visibility before it), by the grammar above and
//! nothing else: a text that is not exactly that form after its `extern crate` is refused by
//! name ([`ExternCrateUnread`]), never guessed. A raw identifier binds the identifier without
//! its `r#` (the Reference, "Identifiers" [ident.raw.intro]: "The `r#` prefix is not included
//! as part of the actual identifier").

/// What an `extern crate` item's text binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternBinding<'t> {
    /// The name bound in the type namespace of the block.
    Named(&'t str),
    /// `as _`: the crate is linked, no name is bound.
    Unbound,
}

/// An `extern crate` text not read: which part of the grammar it left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternCrateUnread {
    /// No whole words `extern crate` in the text.
    NoKeywords,
    /// No identifier (or `self`) after `extern crate`.
    NoCrateName,
    /// After the crate's name: neither `;` nor `as` followed by an identifier or `_` and `;`.
    NotTheForm,
    /// `extern crate self;` binds no name: the language requires `as`.
    SelfWithoutAs,
}

impl ExternCrateUnread {
    /// The words of the refusal (a `Why::Unsupported` form).
    pub fn form(self) -> &'static str {
        match self {
            ExternCrateUnread::NoKeywords => "an extern crate in a block whose text has no `extern crate`",
            ExternCrateUnread::NoCrateName => "an extern crate in a block with no crate name after `extern crate`",
            ExternCrateUnread::NotTheForm => "an extern crate in a block that is not `extern crate NAME (as NAME|_)?;`",
            ExternCrateUnread::SelfWithoutAs => "`extern crate self;` in a block without `as` (the Reference requires a name)",
        }
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()
}

/// The identifier at the start of `s` (an `r#` prefix dropped) and the rest after it.
fn ident(s: &str) -> Option<(&str, &str)> {
    let body = s.strip_prefix("r#").unwrap_or(s);
    let len = body.bytes().take_while(|b| is_ident_byte(*b)).count();
    if len == 0 || body.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    Some((&body[..len], &body[len..]))
}

/// The offset after the first whole-word `extern` followed, past whitespace, by the whole word
/// `crate`.
fn after_keywords(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(i) = text[from..].find("extern") {
        let at = from + i;
        from = at + "extern".len();
        let before = at == 0 || !is_ident_byte(bytes[at - 1]);
        if !before {
            continue;
        }
        let rest = &text[from..];
        let gap = rest.len() - rest.trim_start().len();
        if gap == 0 {
            continue;
        }
        let rest = &rest[gap..];
        if let Some(after) = rest.strip_prefix("crate")
            && !after.bytes().next().is_some_and(is_ident_byte)
        {
            return Some(from + gap + "crate".len());
        }
    }
    None
}

/// What the `extern crate` item `text` binds (see the module's doc for the grammar).
pub fn extern_crate_binding(text: &str) -> Result<ExternBinding<'_>, ExternCrateUnread> {
    let at = after_keywords(text).ok_or(ExternCrateUnread::NoKeywords)?;
    let rest = text[at..].trim_start();
    let (krate, rest) = ident(rest).ok_or(ExternCrateUnread::NoCrateName)?;
    let rest = rest.trim_start();
    if let Some(end) = rest.strip_prefix(';') {
        if !end.trim().is_empty() {
            return Err(ExternCrateUnread::NotTheForm);
        }
        if krate == "self" {
            return Err(ExternCrateUnread::SelfWithoutAs);
        }
        return Ok(ExternBinding::Named(krate));
    }
    let Some(after_as) = rest.strip_prefix("as") else { return Err(ExternCrateUnread::NotTheForm) };
    if after_as.bytes().next().is_some_and(is_ident_byte) {
        return Err(ExternCrateUnread::NotTheForm);
    }
    let after_as = after_as.trim_start();
    let (bound, end) = ident(after_as).ok_or(ExternCrateUnread::NotTheForm)?;
    let end = end.trim_start().strip_prefix(';').ok_or(ExternCrateUnread::NotTheForm)?;
    if !end.trim().is_empty() {
        return Err(ExternCrateUnread::NotTheForm);
    }
    Ok(if bound == "_" { ExternBinding::Unbound } else { ExternBinding::Named(bound) })
}

#[cfg(test)]
mod tests {
    use super::{ExternBinding, ExternCrateUnread, extern_crate_binding};

    /// The Reference's three forms (named, renamed, `as _`), with an attribute and a
    /// visibility before; a text that reads `extern crate` inside an attribute's string is
    /// refused, not misread. Prints nothing.
    #[test]
    fn the_grammar_and_nothing_else() {
        assert_eq!(extern_crate_binding("extern crate core;"), Ok(ExternBinding::Named("core")));
        assert_eq!(extern_crate_binding("#[allow(unused)] pub(crate) extern crate alloc as __alloc ;"), Ok(ExternBinding::Named("__alloc")));
        assert_eq!(extern_crate_binding("extern crate self as me;"), Ok(ExternBinding::Named("me")));
        assert_eq!(extern_crate_binding("extern crate r#core as r#c;"), Ok(ExternBinding::Named("c")));
        assert_eq!(extern_crate_binding("extern crate std as _;"), Ok(ExternBinding::Unbound));
        assert_eq!(extern_crate_binding("extern crate self;"), Err(ExternCrateUnread::SelfWithoutAs));
        assert_eq!(extern_crate_binding("#[doc = \"extern crate a;\"] extern crate b;"), Err(ExternCrateUnread::NotTheForm));
        assert_eq!(extern_crate_binding("externcrate a;"), Err(ExternCrateUnread::NoKeywords));
    }
}
