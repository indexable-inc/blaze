//! The whole network as a capability a `main` names (D104 B1 piece 2, step 0; decided by the
//! coordinator 2026-11-16, port27/d104-b1-entries.md): a program that learns where it connects
//! only at run time (the `blaze` command's cache origin, from `--origin`) takes `Network` in its
//! signature and narrows it ITSELF, handing down only the narrowed [`Net`]: narrowing in code is
//! the capability way. A program whose endpoint is known when it is written takes a scoped
//! [`Net`] instead (`--grant api=host:443`, or the attribute's default).
//!
//! `Network` is not a `Net`: it opens no connection itself, so nothing below the entry reaches
//! an endpoint nobody narrowed to. It is a whole capability (grant.rs `whole!`, by the offer's
//! name `net` in `Offers::NAMES`, as every whole capability is): filled from the process's own world on an ordinary start, and by
//! `--grant <name>=whole` on a run spec.

use crate::{
    io::{Io, Missing},
    net::{Addr, Net},
};

/// Every endpoint, to connect to or listen on, until narrowed.
pub struct Network {
    net: Net,
}

impl Network {
    /// Connects only to `at`, and listens nowhere: what the program hands down.
    pub fn connect_only(&self, at: &Addr) -> Net {
        self.net.connect_only(at)
    }

    /// Listens only on `at`, and connects nowhere.
    pub fn listen_only(&self, at: &Addr) -> Net {
        self.net.listen_only(at)
    }
}

impl Io {
    /// The whole network of this world, or what it does not offer: the one way a `Network` is
    /// made (its grant, grant.rs `whole!`, calls it).
    pub fn need_network(&self) -> Result<Network, Missing> {
        self.need_net().map(|net| Network { net })
    }
}

#[cfg(test)]
mod tests {
    use super::Network;
    use crate::{
        cx::Cx,
        entry::{Status, launch_in},
        grant::{Grant, Manifest, Param},
        io::Io,
    };

    /// A program that takes the whole network as `web`.
    static MANIFEST: Manifest = Manifest::new(&[Param::new("web", <Network as Grant>::KIND)]);

    fn reach(_cx: &Cx, web: Network) -> Status {
        let _ = web;
        Status::OK
    }

    /// An ordinary start (no run spec: a person typed the command) fills a whole capability
    /// from the process's own world (coordinator's decision 1, 2026-11-16); a spec start that
    /// grants nothing is refused before `main`. Without the `Started::Ordinary` arm in
    /// grant.rs `whole!` the first assertion fails (`web` has no grant). Passes as
    /// `network::tests::an_ordinary_start_fills_the_whole_network ... ok`.
    #[test]
    fn an_ordinary_start_fills_the_whole_network() {
        let alloc = blaze_own::testing::heap().shared();
        let mut said = std::string::String::new();
        let status = launch_in(&MANIFEST, None, Io::sim(1), &mut said, crate::launch_body!(reach; web: Network));
        assert_eq!((status, said.as_str()), (Status::OK, ""));

        // Control: started with a spec that grants nothing, the same program is refused.
        let spec = crate::spec::RunSpec::defaults(
            blaze_own::Vec::new_in(alloc),
            blaze_own::Vec::new_in(alloc),
            blaze_own::String::from_str_in("/work", alloc),
            alloc,
        );
        let fd3 = crate::spec::delivery(&spec, alloc);
        let mut said = std::string::String::new();
        let status = launch_in(&MANIFEST, Some(fd3.as_slice()), Io::sim(1), &mut said, crate::launch_body!(reach; web: Network));
        assert_eq!(status, Status::REFUSED, "{said}");
    }
}
