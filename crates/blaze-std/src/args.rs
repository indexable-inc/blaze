//! The command line (docs/v2/plan/design/std.md, `Args`): the world's arguments. A native
//! world's are views over the process's start block, where the kernel left them (std.md, "What
//! a program pays for at start"); a run spec's or a simulated world's are owned text. There is
//! no arguments function: a holder of `Args` reads them, no one else.

use alloc::{string::String, sync::Arc, vec::Vec};

use blaze_own::SharedAlloc;

/// A world's command line, the program's own name first: what an [`Args`] reads.
#[derive(Clone)] // clone: a refcount, or a `&'static`
pub struct ArgList(Source);

#[derive(Clone)]
enum Source {
    /// The process's own, checked UTF-8 at start ([`crate::rt::block`]), less the `skip`
    /// arguments after the program's name the launcher took (an ordinary start's `--grant`s).
    Process { args: &'static [&'static str], skip: usize },
    Given(Arc<[String]>),
    Owned(blaze_own::Arc<blaze_own::Vec<blaze_own::String<SharedAlloc<'static>>, SharedAlloc<'static>>, SharedAlloc<'static>>),
}

impl ArgList {
    /// A command line of owned text: a run spec's, or a test's.
    pub fn given(args: Vec<String>) -> ArgList {
        // alloc: once per world, its command line.
        ArgList(Source::Given(args.into()))
    }

    /// A command line built from borrowed slices in an explicit shared allocator (e.g. a resident
    /// daemon request's argv).
    pub fn from_slices_in(args: &[&str], alloc: SharedAlloc<'static>) -> ArgList {
        let mut owned: blaze_own::Vec<blaze_own::String<SharedAlloc<'static>>, SharedAlloc<'static>> =
            blaze_own::Vec::with_capacity_in(args.len(), alloc);
        for &arg in args {
            owned.push(blaze_own::String::from_str_in(arg, alloc));
        }
        ArgList(Source::Owned(blaze_own::Arc::new_in(owned, alloc)))
    }

    /// The process's own, borrowed from its start block.
    pub(crate) fn process(block: &'static crate::rt::block::Block) -> ArgList {
        ArgList::process_after(block, 0)
    }

    /// The process's own less the `skip` arguments after its name, which the launcher took.
    pub(crate) fn process_after(block: &'static crate::rt::block::Block, skip: usize) -> ArgList {
        ArgList(Source::Process { args: &block.args, skip })
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn get(&self, at: usize) -> Option<&str> {
        match &self.0 {
            Source::Process { args, skip } => match at {
                0 => args.first().copied(),
                _ => args.get(at.saturating_add(*skip)).copied(),
            },
            Source::Given(args) => args.get(at).map(String::as_str),
            Source::Owned(args) => args.get(at).map(blaze_own::String::as_str),
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn len(&self) -> usize {
        match &self.0 {
            Source::Process { args, skip } => args.len().saturating_sub(*skip),
            Source::Given(args) => args.len(),
            Source::Owned(args) => args.len(),
        }
    }
}

/// The command line, the program's own name first.
#[derive(Clone)] // clone: a refcount
pub struct Args {
    args: ArgList,
}

impl Args {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(args: ArgList) -> Args {
        Args { args }
    }

    /// Wraps an [`ArgList`] as an [`Args`] capability (e.g. a resident daemon constructing one
    /// from a client request's decoded argv).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn from_list(args: ArgList) -> Args {
        Args { args }
    }

    /// The argument at `at`, 0 being the program's name.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(&self, at: usize) -> Option<&str> {
        self.args.get(at)
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Each argument, in order.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn each(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).filter_map(|at| self.get(at))
    }
}

impl core::fmt::Debug for ArgList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries((0..self.len()).filter_map(|at| self.get(at))).finish()
    }
}

impl core::fmt::Debug for Args {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.each()).finish()
    }
}
