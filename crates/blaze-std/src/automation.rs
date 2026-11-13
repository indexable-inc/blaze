//! Driving other applications (docs/v2/plan/design/std.md, "The rest of the set",
//! `Automation`): synthesize input into them, read their UI. Narrowed to one application by
//! [`Automation::only`], which never widens. A program's own windows are
//! [`crate::window::Window`].
//!
//! Kept minimal: the type, its narrowing, and one operation (a key pressed in an application).
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::apps`]): blaze-core has no
//! effect for an accessibility or automation interface.

use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    name::AppName,
    narrow::Allowed,
    term::KeyEvent,
};

/// An automation call that failed: the call (a key pressed), the application and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct AutomationError {
    pub app: AppName,
    pub kind: Refused,
}

impl fmt::Display for AutomationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::automation press of a key in application {}: {}", self.app, self.kind)
    }
}

impl core::error::Error for AutomationError {}

/// Other applications: from [`Io::automation`].
#[derive(Clone)]
pub struct Automation {
    io: Io,
    allowed: Allowed<AppName>,
}

impl Automation {
    pub(crate) fn new(io: Io) -> Automation {
        Automation { io, allowed: Allowed::Any }
    }

    /// Only the application `app`, if this reaches it already.
    pub fn only(&self, app: &AppName) -> Automation {
        Automation { io: self.io.clone(), allowed: self.allowed.one(app, self.io.plant()) }
    }

    /// `key` pressed in the application `app`.
    pub fn press(&self, cx: &Cx, app: &AppName, key: KeyEvent) -> Result<(), AutomationError> {
        // clone: an error's name of its application, a refcount.
        let refuse = |kind| AutomationError { app: app.clone(), kind };
        if !self.allowed.allows(app) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            if !host.apps.contains(app) {
                return Err(Refused::NotFound);
            }
            // clone: the simulated host's record, a refcount.
            hosted::record(&mut host.pressed, (app.clone(), key))
        })
        .map_err(refuse)
    }
}

impl fmt::Debug for Automation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Automation({:?})", self.allowed)
    }
}
