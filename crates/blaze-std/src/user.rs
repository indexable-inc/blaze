//! Who is running (docs/v2/plan/design/std.md, "The rest of the set", `User`): the user's
//! name, ids and home. Not in `Env`, so reading variables does not reveal it. The home is a
//! [`Dir`], not a path string: the authority, stored, where a name would have to meet an `Fs`
//! later.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::user`]): blaze-core has no
//! effect that asks who the process runs as.

use core::fmt;

use crate::{
    fs::{Dir, ReadWrite},
    io::Io,
    name::{NameError, UserName},
    path::{Path, PathError},
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
    /// `home/sim`. Its name and home are kept on the global heap: an error of no room when it
    /// has none (the name and the path themselves pass).
    pub fn simulated() -> Result<UserFacts, UserFactsError> {
        Ok(UserFacts {
            name: UserName::new("sim").map_err(UserFactsError::Name)?,
            uid: 1000,
            gid: 1000,
            home: Path::new("home/sim").map_err(UserFactsError::Home)?,
        })
    }
}

/// Why [`UserFacts::simulated`] made none: its name or its home was refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum UserFactsError {
    Name(NameError),
    Home(PathError),
}

impl fmt::Display for UserFactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserFactsError::Name(error) => write!(f, "bstd::user the simulated user's name: {error}"),
            UserFactsError::Home(error) => write!(f, "bstd::user the simulated user's home: {error}"),
        }
    }
}

impl core::error::Error for UserFactsError {}

/// The user: from [`Io::user`].
#[derive(Clone)]
pub struct User {
    io: Io,
    facts: UserFacts,
}

impl User {
    pub(crate) fn new(io: Io, facts: UserFacts) -> User {
        User { io, facts }
    }

    pub fn name(&self) -> &UserName {
        &self.facts.name
    }

    pub fn uid(&self) -> u32 {
        self.facts.uid
    }

    pub fn gid(&self) -> u32 {
        self.facts.gid
    }

    /// The home directory, read and write.
    pub fn home(&self) -> Dir<ReadWrite> {
        Dir::at(&self.io, self.facts.home.to_name())
    }
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "User({:?})", self.facts)
    }
}
