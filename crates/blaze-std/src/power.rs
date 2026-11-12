//! Power (docs/v2/plan/design/std.md, "The rest of the set", `Power`): read the battery, keep
//! the machine awake.
//!
//! **Finishing.** Keeping the machine awake is a linear guard, [`Awake`]: a hold never let go
//! drains a battery with nothing to show for it, so leaving the guard is a compile error and
//! it ends with [`Awake::end`]. A panic that unwinds past one lets the hold go in its `Drop`.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::battery`], `awake`):
//! blaze-core has no effect for a power assertion or the battery.

use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
};

/// The battery's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    /// 0 to 100.
    pub percent: u8,
    pub charging: bool,
}

/// What a power call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerOp {
    Battery,
    KeepAwake,
    EndAwake,
}

/// A power call that failed: the call (its thing is the machine) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct PowerError {
    pub op: PowerOp,
    pub kind: Refused,
}

impl fmt::Display for PowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::power {:?} of the machine: {}", self.op, self.kind)
    }
}

impl core::error::Error for PowerError {}

/// The machine's power: from [`Io::power`].
#[derive(Clone)]
pub struct Power {
    io: Io,
}

impl Power {
    #[blaze::nopanic]
pub(crate) fn new(io: Io) -> Power {
        Power { io }
    }

    pub fn battery(&self, cx: &Cx) -> Result<Battery, PowerError> {
        hosted::call(&self.io, cx, |host| Ok(host.battery)).map_err(|kind| PowerError { op: PowerOp::Battery, kind })
    }

    /// The machine kept awake until the guard is ended.
    pub fn keep_awake(&self, cx: &Cx) -> Result<Awake, PowerError> {
        let held = hosted::call(&self.io, cx, |host| {
            host.awake += 1;
            Ok(())
        });
        match held {
            Ok(()) => Ok(Awake { hold: Hold { io: self.io.clone(), on: true } }),
            Err(kind) => Err(PowerError { op: PowerOp::KeepAwake, kind }),
        }
    }
}

impl fmt::Debug for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Power")
    }
}

/// The machine held awake, ended with [`Awake::end`]. Linear (see the module's doc).
#[blaze::linear]
#[must_use = "a hold on the machine's sleep is ended with end(cx)"]
pub struct Awake {
    hold: Hold,
}

/// A hold's abandonment path.
struct Hold {
    io: Io,
    /// Whether the hold is still to be let go: cleared by [`Awake::end`].
    on: bool,
}

impl Hold {
    fn release(&mut self) -> Option<()> {
        self.on = false;
        self.io.script(|host| host.awake = host.awake.saturating_sub(1))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if self.on {
            let _ = self.release();
        }
    }
}

impl Awake {
    /// The hold let go, whatever `cx` says. The one way an `Awake` ends.
    #[blaze::consumes]
    pub fn end(mut self, cx: &Cx) -> Result<(), PowerError> {
        cx.event("bstd::power: hold on sleep ended");
        match self.hold.release() {
            Some(()) => Ok(()),
            None => Err(PowerError { op: PowerOp::EndAwake, kind: Refused::NoProvider }),
        }
    }
}

impl fmt::Debug for Awake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Awake")
    }
}
