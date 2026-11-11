//! Windows (docs/v2/plan/design/std.md, "The rest of the set", `Window`): a surface to draw
//! on and the input sent to it. Narrowed to one window by [`Window::only`], which never
//! widens. Input to *other* applications' windows is [`crate::automation::Automation`].
//!
//! Kept minimal: a window's surface size and its event stream. Drawing is `Gpu`'s.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::windows`]): blaze-core has no
//! effect for a window system.

use alloc::{collections::VecDeque, vec::Vec};
use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    narrow::Allowed,
    term::KeyEvent,
};

/// A window's id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct WindowId(pub u32);

/// A surface's size in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
}

/// Input sent to a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowEvent {
    Resized(Surface),
    Key(KeyEvent),
    Pointer { x: u32, y: u32, down: bool },
    /// The user asked for it to close.
    Closed,
}

/// A window of the simulated host.
#[derive(Debug)]
pub struct SimWindow {
    pub id: WindowId,
    pub surface: Surface,
    /// The events it will be sent, in order.
    pub events: VecDeque<WindowEvent>,
    /// What a capture of it gives ([`crate::device::Screen`]).
    pub pixels: Vec<u8>,
}

/// What a window call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowOp {
    Surface,
    Event,
}

/// A window call that failed: the call, the window and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct WindowError {
    pub op: WindowOp,
    pub window: WindowId,
    pub kind: Refused,
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::window {:?} of window {}: {}", self.op, self.window.0, self.kind)
    }
}

impl core::error::Error for WindowError {}

/// The program's windows: from [`Io::windows`].
#[derive(Clone)]
pub struct Window {
    io: Io,
    allowed: Allowed<WindowId>,
}

impl Window {
    pub(crate) fn new(io: Io) -> Window {
        Window { io, allowed: Allowed::Any }
    }

    /// Only the window `window`, if this reaches it already.
    pub fn only(&self, window: WindowId) -> Window {
        Window { io: self.io.clone(), allowed: self.allowed.one(&window, self.io.plant()) }
    }

    /// The size of `window`'s surface now.
    pub fn surface(&self, cx: &Cx, window: WindowId) -> Result<Surface, WindowError> {
        let refuse = |kind| WindowError { op: WindowOp::Surface, window, kind };
        if !self.allowed.allows(&window) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| match host.windows.iter().find(|it| it.id == window) {
            Some(found) => Ok(found.surface),
            None => Err(Refused::NotFound),
        })
        .map_err(refuse)
    }

    /// The next event sent to `window`: waits for it until `cx`'s deadline.
    pub fn event(&self, cx: &Cx, window: WindowId) -> Result<WindowEvent, WindowError> {
        let refuse = |kind| WindowError { op: WindowOp::Event, window, kind };
        if !self.allowed.allows(&window) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::wait(&self.io, cx, "bstd::window event", |host| {
            match host.windows.iter_mut().find(|it| it.id == window) {
                Some(found) => found.events.pop_front().map(Ok),
                None => Some(Err(Refused::NotFound)),
            }
        })
        .map_err(refuse)
    }
}

impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Window({:?})", self.allowed)
    }
}
