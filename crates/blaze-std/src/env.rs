//! Environment variables (docs/v2/plan/design/std.md, `Env`): the world's, read once when it
//! was made (handed in by the entry until `bstd::rt` reads the start block), and narrowed to
//! named ones by [`Env::only`], which never widens.
//!
//! A variable is named by a [`VarName`], never a string ("Names are typed, and authority is
//! stored"): checked as it is made, by [`VarName::new`] or, for a literal, by
//! [`crate::var!`] as the crate compiles.

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::fmt;

use crate::Plant;

/// An environment variable's name: not empty, with no `=` and no NUL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount, no allocation
pub struct VarName(Arc<str>);

/// What [`VarName::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct VarNameError {
    pub name: String,
    pub why: VarNameWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VarNameWhy {
    Empty,
    /// It holds a `=`, which ends a name in the environment block.
    Equals,
    /// It holds a NUL, which ends the environment block's entry.
    Nul,
}

impl VarNameWhy {
    const fn says(self) -> &'static str {
        match self {
            VarNameWhy::Empty => "is empty",
            VarNameWhy::Equals => "holds a `=`, which would end the name in the environment block",
            VarNameWhy::Nul => "holds a NUL, which would end its entry in the environment block",
        }
    }
}

impl fmt::Display for VarNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "environment variable name {:?} {}", self.name, self.why.says())
    }
}

impl core::error::Error for VarNameError {}

/// Why `raw` is no variable name, if it is not. A `const fn`, so [`crate::var!`] checks a
/// literal as the crate compiles with the same rule [`VarName::new`] checks at run time.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn refused(raw: &str) -> Option<VarNameWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(VarNameWhy::Empty);
    }
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'=' => return Some(VarNameWhy::Equals),
            0 => return Some(VarNameWhy::Nul),
            _ => {}
        }
        at += 1;
    }
    None
}

/// [`refused`], as a compile-time panic: what [`crate::var!`] evaluates in a `const`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn check_literal(raw: &str) {
    if let Some(why) = refused(raw) {
        panic!("{}", why.says());
    }
}

impl VarName {
    #[blaze::pure]
#[blaze::nonblocking]
pub fn new(raw: &str) -> Result<VarName, VarNameError> {
        if let Some(why) = refused(raw) {
            // alloc: an error's message.
            return Err(VarNameError { name: raw.to_owned(), why });
        }
        // alloc: once per name, its text.
        Ok(VarName(Arc::from(raw)))
    }

    #[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for VarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VarName({:?})", self.as_str())
    }
}

impl TryFrom<&str> for VarName {
    type Error = VarNameError;

    #[blaze::pure]
#[blaze::nonblocking]
fn try_from(raw: &str) -> Result<VarName, VarNameError> {
        VarName::new(raw)
    }
}

/// A [`VarName`] from a string literal, checked as the crate compiles: `var!("HOME")`. A bad
/// literal (`var!("A=B")`) is a compile error saying why.
#[macro_export]
macro_rules! var {
    ($raw:literal) => {{
        const _: () = $crate::env::check_literal($raw);
        $crate::env::VarName::new($raw).expect("a var! literal was checked as the crate compiled")
    }};
}

/// A variable an [`Env`] was asked for and may not read: it was narrowed away from it
/// ([`Env::only`]). Whether the variable is set is not said.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct NotAllowed {
    pub name: VarName,
}

impl fmt::Display for NotAllowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::env get of {}: this Env was narrowed to other variables (Env::only) and may not read it", self.name)
    }
}

impl core::error::Error for NotAllowed {}

/// The variables a holder may read.
#[derive(Clone)]
pub struct Env {
    vars: Arc<[(String, String)]>,
    /// The names it may read; `None` for every one.
    allowed: Option<Arc<[VarName]>>,
    plant: Plant,
}

impl Env {
    #[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn new(vars: Arc<[(String, String)]>, plant: Plant) -> Env {
        Env { vars, allowed: None, plant }
    }

    /// The variable `name`: its value, `None` when it is not set, or [`NotAllowed`] when
    /// this may not read it. The two are never one answer: a program that was narrowed away
    /// from a variable does not take it for unset.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(&self, name: &VarName) -> Result<Option<&str>, NotAllowed> {
        if !self.may_read(name) {
            // clone: an error's name of its variable, a refcount.
            return Err(NotAllowed { name: name.clone() });
        }
        Ok(self.vars.iter().find(|(it, _)| it.as_str() == name.as_str()).map(|(_, value)| value.as_str()))
    }

    /// Whether this may read `name`, set or not.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn may_read(&self, name: &VarName) -> bool {
        self.may_read_str(name.as_str())
    }

    fn may_read_str(&self, name: &str) -> bool {
        self.allowed.as_deref().is_none_or(|allowed| allowed.iter().any(|it| it.as_str() == name))
    }

    /// Only the variables of `names` this may read already: never one more.
    #[blaze::nonblocking]
    pub fn only(&self, names: &[VarName]) -> Env {
        let kept = names.iter().filter(|name| match self.plant {
            Plant::EnvOnlyReplaces => true,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned => self.may_read(name),
        });
        // alloc: once per narrowing, the names kept (each a refcount).
        let allowed: Arc<[VarName]> = kept.cloned().collect();
        Env { vars: Arc::clone(&self.vars), allowed: Some(allowed), plant: self.plant }
    }

    /// Each variable this may read that is set, in the world's order.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn each(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .filter(|(name, _)| self.may_read_str(name))
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.allowed {
            None => f.write_str("Env(all)"),
            Some(allowed) => write!(f, "Env(only {allowed:?})"),
        }
    }
}
