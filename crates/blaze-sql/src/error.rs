//! Why a statement was refused. Each step has its own error in its own module; this is their
//! sum, which [`crate::check`] returns. Every message names the thing it is about (the token,
//! the column, the parameter) and the rule that refused it, with its place in the text.

#![blaze::sans_io]

use core::fmt;

use crate::infer::TypeError;
use crate::parse::SyntaxError;

#[derive(Debug)]
pub enum Error<'s> {
    Syntax(SyntaxError<'s>),
    Type(TypeError<'s>),
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(error) => error.fmt(f),
            Error::Type(error) => error.fmt(f),
        }
    }
}
