//! The token builder: generated code is written as text, buffered and lexed once per run of
//! text, and the item's own tokens (its types, bounds and names) are spliced in as they came,
//! so an error in a field's type points at the field. A delimiter in the text opens or closes a
//! group, so a call can be written as `serialize_struct(__serializer, ` then the name's literal
//! then `)`, with the literal inside the call's group.

use std::{panic::Location, str::FromStr as _};

use proc_macro::{Delimiter, Group, Ident, Literal, Span, TokenStream, TokenTree};

/// The line of the generator that wrote some text, named when that text is refused.
type Site = &'static Location<'static>;

/// A group opened by text and not closed yet.
struct Open {
    delimiter: Delimiter,
    site: Site,
    text: String,
    tokens: Vec<TokenTree>,
}

/// Text the builder refused: where the generator wrote it, the text, and why.
struct Refused {
    site: Site,
    text: String,
    why: &'static str,
}

#[derive(Default)]
pub(crate) struct Out {
    tokens: Vec<TokenTree>,
    open: Vec<Open>,
    text: String,
    /// The site of the run of text in `text`.
    site: Option<Site>,
    /// The first text refused, which [`Out::finish`] reports.
    refused: Option<Refused>,
}

impl Out {
    /// Appends `text`, Rust source with no string or char literals (those are [`Out::str`]):
    /// each `(`, `[` or `{` in it opens a group, closed by the same delimiter in this or a later
    /// text of this `Out`.
    #[track_caller]
    pub(crate) fn s(&mut self, text: &str) -> &mut Self {
        let site = Location::caller();
        let mut run = 0;
        for (at, ch) in text.char_indices() {
            let delimiter = match ch {
                '(' | ')' => Delimiter::Parenthesis,
                '[' | ']' => Delimiter::Bracket,
                '{' | '}' => Delimiter::Brace,
                _ => continue,
            };
            self.text(&text[run..at], site);
            run = at + 1;
            self.flush();
            if matches!(ch, '(' | '[' | '{') {
                // alloc: the text that opened a group, for the error if nothing closes it
                self.open.push(Open { delimiter, site, text: text.to_owned(), tokens: Vec::new() });
            } else {
                self.close(delimiter, site, text);
            }
        }
        self.text(&text[run..], site);
        self.text(" ", site);
        self
    }

    pub(crate) fn tokens(&mut self, tokens: &TokenStream) -> &mut Self {
        self.flush();
        self.top().extend(tokens.clone());
        self
    }

    pub(crate) fn ident(&mut self, ident: &Ident) -> &mut Self {
        self.flush();
        self.top().push(TokenTree::Ident(ident.clone()));
        self
    }

    /// A string literal of `text`.
    pub(crate) fn str(&mut self, text: &str) -> &mut Self {
        self.flush();
        self.top().push(TokenTree::Literal(Literal::string(text)));
        self
    }

    pub(crate) fn usize(&mut self, n: usize) -> &mut Self {
        self.flush();
        self.top().push(TokenTree::Literal(Literal::usize_unsuffixed(n)));
        self
    }

    pub(crate) fn u32(&mut self, n: u32) -> &mut Self {
        self.flush();
        self.top().push(TokenTree::Literal(Literal::u32_unsuffixed(n)));
        self
    }

    pub(crate) fn group(
        &mut self,
        delimiter: Delimiter,
        inside: impl FnOnce(&mut Out),
    ) -> &mut Self {
        self.flush();
        let mut inner = Out::default();
        inside(&mut inner);
        match inner.into_tokens() {
            Ok(stream) => self.top().push(TokenTree::Group(Group::new(delimiter, stream))),
            Err(refused) => self.refuse(refused),
        }
        self
    }

    pub(crate) fn brace(&mut self, inside: impl FnOnce(&mut Out)) -> &mut Self {
        self.group(Delimiter::Brace, inside)
    }

    pub(crate) fn paren(&mut self, inside: impl FnOnce(&mut Out)) -> &mut Self {
        self.group(Delimiter::Parenthesis, inside)
    }

    /// The tokens written, or a `compile_error!` naming the generator's line and the text it
    /// wrote that was refused: a bug of this derive, reported where the derive is.
    pub(crate) fn finish(self) -> TokenStream {
        match self.into_tokens() {
            Ok(stream) => stream,
            Err(Refused { site, text, why }) => crate::parse::Error::new(
                Span::call_site(),
                format!(
                    "blaze-serde-derive's generator at {}:{}:{} wrote `{text}`, which {why}: a bug of the derive",
                    site.file(),
                    site.line(),
                    site.column(),
                ),
            )
            .into_compile_error(),
        }
    }

    fn into_tokens(mut self) -> Result<TokenStream, Refused> {
        self.flush();
        if let Some(open) = self.open.pop() {
            self.refuse(Refused {
                site: open.site,
                text: open.text,
                why: "opens a delimiter nothing closes",
            });
        }
        match self.refused {
            None => Ok(self.tokens.into_iter().collect()),
            Some(refused) => Err(refused),
        }
    }

    fn top(&mut self) -> &mut Vec<TokenTree> {
        match self.open.last_mut() {
            Some(open) => &mut open.tokens,
            None => &mut self.tokens,
        }
    }

    fn text(&mut self, text: &str, site: Site) {
        if self.text.is_empty() {
            self.site = Some(site);
        }
        self.text.push_str(text);
    }

    fn close(&mut self, delimiter: Delimiter, site: Site, text: &str) {
        let why = match self.open.pop() {
            Some(open) if open.delimiter == delimiter => {
                let group = Group::new(delimiter, open.tokens.into_iter().collect());
                self.top().push(TokenTree::Group(group));
                return;
            }
            Some(_) => "closes a delimiter other than the one open",
            None => "closes a delimiter this builder did not open",
        };
        // alloc: an error
        self.refuse(Refused { site, text: text.to_owned(), why });
    }

    fn flush(&mut self) {
        let (Some(site), false) = (self.site, self.text.is_empty()) else { return };
        match TokenStream::from_str(&self.text) {
            Ok(stream) => self.top().extend(stream),
            Err(_) => {
                // alloc: an error
                let text = self.text.clone();
                self.refuse(Refused { site, text, why: "does not lex" });
            }
        }
        self.text.clear();
    }

    fn refuse(&mut self, refused: Refused) {
        self.refused.get_or_insert(refused);
    }
}
