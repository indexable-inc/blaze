//! The command line (docs/v2/plan/design/std.md, `Args`): the world's arguments, read once
//! when it was made (handed in by the entry until `bstd::rt` reads the start block). There is
//! no arguments function: a holder of `Args` reads them, no one else.

use alloc::{string::String, sync::Arc};

/// The command line, the program's own name first.
#[derive(Clone)] // clone: a refcount
pub struct Args {
    args: Arc<[String]>,
}

impl Args {
    pub(crate) fn new(args: Arc<[String]>) -> Args {
        Args { args }
    }

    /// The argument at `at`, 0 being the program's name.
    pub fn get(&self, at: usize) -> Option<&str> {
        self.args.get(at).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.args.len()
    }

    pub fn is_empty(&self) -> bool {
        self.args.is_empty()
    }

    /// Each argument, in order.
    pub fn each(&self) -> impl Iterator<Item = &str> {
        self.args.iter().map(String::as_str)
    }
}

impl core::fmt::Debug for Args {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.each()).finish()
    }
}
