//! What an expansion is kept by in the store, besides its call ([`proc_macro_api::kept::given`]) and
//! its macro's code ([`code`](crate::code)): what it read, as [`os`](crate::os) traced it,
//! looked up again now. A macro reads the calling crate's environment, never blaze's, and the
//! machine's files, so that is what is looked at.

use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

use base_db::Env;
use blaze_usercode::{
    inputs::{self, Files, Now},
    trace::{Entry, Inputs, Names},
};

/// The inputs of an expansion for the calling crate's `env`, as they are now. A variable is
/// looked up where it is, so a hit allocates nothing; the whole environment is made only for
/// a run that read all of it.
pub(crate) struct Calling<'a> {
    env: &'a Env,
    made: Option<inputs::Env>,
    /// What file, directory and listing reads are asked through: no variable is read there.
    none: inputs::Env,
}

impl<'a> Calling<'a> {
    pub(crate) fn new(env: &'a Env) -> Calling<'a> {
        Calling { env, made: None, none: inputs::Env::default() }
    }
}

impl Inputs for Calling<'_> {
    type Error = inputs::Unreadable;

    fn entry(&mut self, path: &Path) -> Result<Entry, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.entry(path)
    }

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.file(path)
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.dir(path)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        let name = name.to_str()?;
        self.env.iter().find(|it| it.0 == name).map(|it| OsStr::new(it.1))
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        let env = self.env;
        // alloc: a run that read the whole environment: made once per call, as its key needs.
        self.made
            .get_or_insert_with(|| {
                inputs::Env::new(env.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()) // alloc: as above
            })
            .vars()
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.names(path, out)
    }
}
