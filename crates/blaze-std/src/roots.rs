//! Whom TLS trusts (docs/v2/plan/design/std.md, "The rest of the set", `Roots`): a set of
//! trust anchors, chosen explicitly, so no connection trusts the machine's store unasked.
//! Narrowed to named authorities by [`Roots::only`], which never widens; [`Roots::none`] trusts
//! no one and needs no authority to make.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::roots`]): reading the
//! machine's store needs an effect blaze-core does not have (blaze-http's `roots` reads it
//! ambiently, which is why bstd leaves that out).

use alloc::{sync::Arc, vec::Vec};
use core::fmt;

use crate::{Plant, Stopped, name::Authority, narrow::Allowed};

/// One trust anchor: an authority and its certificate.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: refcounts
pub struct Anchor {
    pub name: Authority,
    /// The certificate, DER.
    pub der: Arc<[u8]>,
}

/// A set of trust anchors: from [`crate::Io::roots`], or [`Roots::none`].
#[derive(Clone)]
pub struct Roots {
    /// The host's anchors; `None` for the empty set, which so needs no allocation.
    anchors: Option<Arc<Vec<Anchor>>>,
    allowed: Allowed<Authority>,
    plant: Plant,
}

impl Roots {
    pub(crate) fn new(anchors: Arc<Vec<Anchor>>, plant: Plant) -> Roots {
        Roots { anchors: Some(anchors), allowed: Allowed::Any, plant }
    }

    /// The empty set: trusts no one. Not authority, so anyone may make it.
    pub fn none() -> Roots {
        Roots { anchors: None, allowed: Allowed::nothing(), plant: Plant::None }
    }

    /// Only the authorities of `names` this trusts already: never one more.
    /// [`Stopped::OutOfMemory`] when the global heap has no room for the narrowed set.
    pub fn only(&self, names: &[Authority]) -> Result<Roots, Stopped> {
        // clone: the anchors, a refcount.
        let anchors = self.anchors.clone();
        Ok(Roots { anchors, allowed: self.allowed.only(names, self.plant)?, plant: self.plant })
    }

    /// Whether `name` is trusted.
    pub fn trusts(&self, name: &Authority) -> bool {
        self.each().any(|anchor| &anchor.name == name)
    }

    /// Each anchor trusted, in the host's order.
    pub fn each(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.iter().flat_map(|anchors| anchors.iter()).filter(|anchor| self.allowed.allows(&anchor.name))
    }

    /// Whether it trusts no one.
    pub fn is_empty(&self) -> bool {
        self.each().next().is_none()
    }
}

impl fmt::Debug for Roots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.each().map(|anchor| &anchor.name)).finish()
    }
}
