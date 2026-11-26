//! Sans-IO, `#![no_std]` Vim modal editing engine with pluggable [`TextBuffer`] and
//! [`TextObjectResolver`] traits.
//!
//! Designed so any blaze text surface — shell prompts (`blaze-shell`), code editors, or
//! structured inspectors — gets full Vim modal editing (`Normal`, `Insert`, `Visual`,
//! `VisualLine`, counts, motions, operators, registers, undo/redo, `.` dot-repeat) while
//! plugging in domain-specific text objects (e.g. shell pipeline stages `i|`/`a|`, path
//! segments `i/`/`a/`, comma arguments `i,`/`a,`, or AST functions/blocks).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod buffer;
pub mod engine;
pub mod objects;

#[cfg(test)]
mod tests;

pub use crate::buffer::{OwnedBuf, TextBuffer};
pub use crate::engine::{Outcome, Register, VimMachine, VimOpts};
pub use crate::objects::{
    BuiltinObjects, ChainObjects, DelimSegmentObject, ObjectScope, ShellObjects,
    TextObjectResolver,
};

/// Active Vim editing mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
}

impl Mode {
    /// Short 3-character status label (`NOR`, `INS`, `VIS`, `V-L`).
    #[inline]
    pub const fn short_label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Visual => "VIS",
            Mode::VisualLine => "V-L",
        }
    }
}

/// Visual cursor geometry recommended for the current Vim state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    /// Full cell block (`Normal`, `Visual`, `VisualLine`).
    Block,
    /// Thin vertical bar (`Insert`).
    Beam,
    /// Bottom underline (`OperatorPending`, `ReplaceChar`, `FindPending`).
    Underline,
}

/// Half-open byte span `[start, end)` inside a UTF-8 [`TextBuffer`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[inline]
    pub const fn new(start: usize, end: usize) -> Self {
        if start <= end {
            Self { start, end }
        } else {
            Self { start: end, end: start }
        }
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Characterwise vs linewise motion/register semantics.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MotionKind {
    #[default]
    Charwise,
    Linewise,
}

/// Pending Vim operator waiting for a motion or text object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    Lowercase,
    Uppercase,
    ToggleCase,
    Indent,
    Dedent,
}

impl Operator {
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            Operator::Delete => "d",
            Operator::Change => "c",
            Operator::Yank => "y",
            Operator::Lowercase => "gu",
            Operator::Uppercase => "gU",
            Operator::ToggleCase => "g~",
            Operator::Indent => ">",
            Operator::Dedent => "<",
        }
    }
}

/// Single-line character search direction & inclusivity (`f`, `F`, `t`, `T`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FindKind {
    /// Forward inclusive (`f`).
    Forward,
    /// Backward inclusive (`F`).
    Backward,
    /// Forward till before (`t`).
    TillForward,
    /// Backward till after (`T`).
    TillBackward,
}

impl FindKind {
    #[inline]
    pub const fn invert(self) -> Self {
        match self {
            FindKind::Forward => FindKind::Backward,
            FindKind::Backward => FindKind::Forward,
            FindKind::TillForward => FindKind::TillBackward,
            FindKind::TillBackward => FindKind::TillForward,
        }
    }
}

/// Modifier flags carried by a [`KeyEvent`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// Logical key pressed by the host application.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCode {
    Char(char),
    Escape,
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
}

/// One normalized keystroke fed into [`VimMachine::feed`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub mods: Mods,
}

impl KeyEvent {
    #[inline]
    pub const fn char(ch: char) -> Self {
        Self { code: KeyCode::Char(ch), mods: Mods { ctrl: false, alt: false, shift: false } }
    }

    #[inline]
    pub const fn ctrl(ch: char) -> Self {
        Self { code: KeyCode::Char(ch), mods: Mods { ctrl: true, alt: false, shift: false } }
    }

    #[inline]
    pub const fn key(code: KeyCode) -> Self {
        Self { code, mods: Mods { ctrl: false, alt: false, shift: false } }
    }
}
