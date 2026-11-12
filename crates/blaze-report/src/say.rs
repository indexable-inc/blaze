//! Writing a [`Text`] as easily as `format!`, with no way to write an untyped fragment:
//!
//! ```ignore
//! say!("built ", Count(81), " units in ", took, " · ", Cmd("blaze slow"), " says why")
//! ```
//!
//! Each argument is a [`Part`], and its type picks its role. A string literal is words. A
//! value that is not words has a type that says what it is ([`Code`], [`Cmd`], [`At`],
//! [`Pkg`], [`Count`], a `Duration`, [`Strong`]), so nothing is put between backticks by hand
//! and no renderer guesses at punctuation later: a terminal draws a command as a command, a
//! pipe gets the backticks, JSON gets the role. A `String` or a `&str` that is not a literal
//! is no [`Part`]: interpolated text must say what it is. Words that are only known at run
//! time and are prose (another program's message) go in as [`Words`], which is the one
//! explicit way to say so.

use std::time::Duration;

use crate::{Span, Text};

/// What can be a part of a [`Text`].
pub trait Part {
    fn span(self) -> Span;
}

/// Words written in the source.
impl Part for &'static str {
    fn span(self) -> Span {
        Span::Plain(self.to_owned()) // alloc: a message's words
    }
}

impl Part for Span {
    fn span(self) -> Span {
        self
    }
}

/// Prose known only at run time: another program's words, a message carried as it came.
pub struct Words<'a>(pub &'a str);
/// Code or an identifier.
pub struct Code<'a>(pub &'a str);
/// A command the reader can run as it is.
pub struct Cmd<'a>(pub &'a str);
/// A file or directory.
pub struct At<'a>(pub &'a str);
/// A package by its name.
pub struct Pkg<'a>(pub &'a str);
/// A count.
pub struct Count(pub u64);
/// What the line is about, said with weight.
pub struct Strong<'a>(pub &'a str);

impl Part for Words<'_> {
    fn span(self) -> Span {
        Span::Plain(self.0.to_owned()) // alloc: a message's words
    }
}

impl Part for Code<'_> {
    fn span(self) -> Span {
        Span::Code(self.0.to_owned()) // alloc: as above
    }
}

impl Part for Cmd<'_> {
    fn span(self) -> Span {
        Span::Command(self.0.to_owned()) // alloc: as above
    }
}

impl Part for At<'_> {
    fn span(self) -> Span {
        Span::Path(self.0.to_owned()) // alloc: as above
    }
}

impl Part for Pkg<'_> {
    fn span(self) -> Span {
        Span::Package { name: self.0.to_owned(), version: None } // alloc: as above
    }
}

impl Part for Count {
    fn span(self) -> Span {
        Span::Count(self.0)
    }
}

impl Part for Strong<'_> {
    fn span(self) -> Span {
        Span::Strong(self.0.to_owned()) // alloc: as above
    }
}

impl Part for Duration {
    fn span(self) -> Span {
        Span::Took(u64::try_from(self.as_nanos()).unwrap_or(u64::MAX))
    }
}

/// A [`Text`] of its arguments, each a [`Part`].
#[macro_export]
macro_rules! say {
    ($($part:expr),* $(,)?) => {
        $crate::Text(::std::vec![$($crate::say::Part::span($part)),*]) // alloc: a message, once
    };
}

/// Every span of `more` at the end of `text`.
pub fn join(mut text: Text, more: Text) -> Text {
    text.0.extend(more.0);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Caps, Roots, render::text_into};

    /// One message, each rendering: a terminal draws the roles and no backtick, a pipe gets
    /// the backticks back where a reader needs them, JSON carries the roles.
    #[test]
    fn one_message_is_drawn_for_each_reader() {
        let text = say!(
            Code("hello::add"),
            " recompiled in ",
            Duration::from_millis(700),
            " · run ",
            Cmd("blaze explain why:input"),
            " · ",
            Count(3),
            " warnings in ",
            At("/w/src/main.rs"),
        );
        assert_eq!(
            text.flat(),
            "`hello::add` recompiled in 0.70s · run `blaze explain why:input` · 3 warnings in /w/src/main.rs"
        );
        let mut drawn = String::new();
        let roots = Roots { workspace: Some("/w"), home: None };
        text_into(&mut drawn, &text, Caps { color: true, links: false, width: None }, &roots);
        assert!(!drawn.contains('`'), "{drawn:?}");
        assert!(drawn.contains("hello::add") && drawn.contains("src/main.rs"), "{drawn:?}");
        let json = sonic_rs::to_string(&text).unwrap();
        assert!(json.contains(r#"{"Code":"hello::add"}"#), "{json}");
        assert!(json.contains(r#"{"Command":"blaze explain why:input"}"#), "{json}");
        assert!(json.contains(r#"{"Took":700000000}"#), "{json}");
    }
}
