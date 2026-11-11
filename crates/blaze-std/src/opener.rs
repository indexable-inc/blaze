//! Handing a URL to the user's default application (docs/v2/plan/design/std.md, "The rest of
//! the set", `Opener`). Narrowed to one scheme by [`Opener::only`], which never widens: an
//! opener of `https` cannot open a `file:`.
//!
//! **Provider.** The simulation only, which records what was opened
//! ([`crate::hosted::SimHost::opened`]): blaze-core has no effect that asks the desktop to open
//! something, and starting `open` or `xdg-open` as a process is not one (a program runs only
//! in a sandbox).

use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    narrow::Allowed,
    url::{Scheme, Url},
};

/// An open that failed: the URL and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct OpenerError {
    pub url: Url,
    pub kind: Refused,
}

impl fmt::Display for OpenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::opener open of {}: {}", self.url, self.kind)
    }
}

impl core::error::Error for OpenerError {}

/// The user's default applications: from [`Io::opener`].
#[derive(Clone)]
pub struct Opener {
    io: Io,
    allowed: Allowed<Scheme>,
}

impl Opener {
    pub(crate) fn new(io: Io) -> Opener {
        Opener { io, allowed: Allowed::Any }
    }

    /// Only URLs of the scheme `scheme`, if this opens them already.
    pub fn only(&self, scheme: &Scheme) -> Opener {
        Opener { io: self.io.clone(), allowed: self.allowed.one(scheme, self.io.plant()) }
    }

    /// `url` handed to the user's default application for its scheme.
    pub fn open(&self, cx: &Cx, url: &Url) -> Result<(), OpenerError> {
        // clone: an error's name of its URL, refcounts.
        let refuse = |kind| OpenerError { url: url.clone(), kind };
        if !self.allowed.allows(url.scheme()) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            // clone: the simulated host's record, refcounts.
            host.opened.push(url.clone());
            Ok(())
        })
        .map_err(refuse)
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Opener({:?})", self.allowed)
    }
}
