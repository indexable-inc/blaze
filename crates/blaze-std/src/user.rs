//! Who is running (docs/v2/plan/design/std.md, "The rest of the set", `User`): the user's
//! name, ids and home. Not in `Env`, so reading variables does not reveal it. The home is a
//! [`Dir`], not a path string: the authority, stored, where a name would have to meet an `Fs`
//! later.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::user`]): blaze-core has no
//! effect that asks who the process runs as.

use core::fmt;

use blaze_core::FileName;

use crate::{
    fs::{Dir, ReadWrite},
    io::Io,
    name::UserName,
    path::Path,
};

/// What a host says of its user.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: plain data and refcounts
pub struct UserFacts {
    pub name: UserName,
    pub uid: u32,
    pub gid: u32,
    /// The home directory, under the driver's root.
    pub home: Path,
}

impl UserFacts {
    /// The simulation's unless a test says otherwise: `sim`, user and group 1000, at
    /// `home/sim`.
    #[blaze::nonblocking]
    pub fn simulated() -> UserFacts {
        UserFacts {
            name: UserName::new("sim").expect("sim is a name"),
            uid: 1000,
            gid: 1000,
            home: Path::new("home/sim").expect("home/sim is a relative path"),
        }
    }
}

/// The user: from [`Io::user`].
#[derive(Clone)]
pub struct User {
    io: Io,
    facts: UserFacts,
}

impl User {
    #[blaze::nopanic]
pub(crate) fn new(io: Io, facts: UserFacts) -> User {
        User { io, facts }
    }

    #[blaze::nopanic]
pub fn name(&self) -> &UserName {
        &self.facts.name
    }

    #[blaze::nopanic]
pub fn uid(&self) -> u32 {
        self.facts.uid
    }

    #[blaze::nopanic]
pub fn gid(&self) -> u32 {
        self.facts.gid
    }

    /// The home directory, read and write.
    pub fn home(&self) -> Dir<ReadWrite> {
        let name: &FileName = self.facts.home.name();
        // clone: the directory's name, a refcount.
        Dir::at(&self.io, name.clone())
    }
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "User({:?})", self.facts)
    }
}
