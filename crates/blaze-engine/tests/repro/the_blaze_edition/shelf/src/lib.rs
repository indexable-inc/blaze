//! A library of the blaze edition: its functions take what they touch.

use std::out::{Out, OutError};

pub fn greet(cx: &Cx, out: &Out) -> Result<(), OutError> {
    out.line(cx, "Hello from the shelf!")
}
