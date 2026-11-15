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

use crate::{Plant, name::Authority, narrow::Allowed};

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
    anchors: Arc<[Anchor]>,
    allowed: Allowed<Authority>,
    plant: Plant,
}

impl Roots {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(anchors: Arc<[Anchor]>, plant: Plant) -> Roots {
        Roots { anchors, allowed: Allowed::Any, plant }
    }

    /// The empty set: trusts no one. Not authority, so anyone may make it.
    #[blaze::nonblocking]
    pub fn none() -> Roots {
        // alloc: once per empty set, its count.
        Roots { anchors: Arc::from(Vec::new()), allowed: Allowed::nothing(), plant: Plant::None }
    }

    /// Only the authorities of `names` this trusts already: never one more.
    #[blaze::nonblocking]
    pub fn only(&self, names: &[Authority]) -> Roots {
        Roots { anchors: Arc::clone(&self.anchors), allowed: self.allowed.only(names, self.plant), plant: self.plant }
    }

    /// Whether `name` is trusted.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn trusts(&self, name: &Authority) -> bool {
        self.each().any(|anchor| &anchor.name == name)
    }

    /// Each anchor trusted, in the host's order.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn each(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.iter().filter(|anchor| self.allowed.allows(&anchor.name))
    }

    /// Whether it trusts no one.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.each().next().is_none()
    }
}

impl fmt::Debug for Roots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.each().map(|anchor| &anchor.name)).finish()
    }
}
