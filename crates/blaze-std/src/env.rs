//! Environment variables (docs/v2/plan/design/std.md, `Env`): the world's, read once when it
//! was made (handed in by the entry until `bstd::rt` reads the start block), and narrowed to
//! named ones by [`Env::only`], which never widens.
//!
//! A variable is named by a [`VarName`], never a string ("Names are typed, and authority is
//! stored"): checked as it is made, by [`VarName::new`] or, for a literal, by
//! [`crate::var!`] as the crate compiles.

use alloc::{alloc::Global, string::String, sync::Arc};
use core::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

use crate::{
    Plant, Stopped,
    mem::{Vec, heap},
};

/// An environment variable's name: not empty, with no `=` and no NUL.
#[derive(Clone)] // clone: a refcount or a pointer, no allocation
pub struct VarName(Text);

/// A name's text: a literal's ([`crate::var!`]), which needs no copy, or one made at run time.
#[derive(Clone)] // clone: a refcount or a pointer, no allocation
enum Text {
    Literal(&'static str),
    Made(Arc<String>),
}

/// What [`VarName::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct VarNameError {
    /// The name refused; empty when its copy found no room ([`VarNameWhy::OutOfMemory`]).
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
    /// The global heap had no room for its text (`asked` bytes), as a name or in this error.
    OutOfMemory { asked: usize },
}

impl VarNameWhy {
    const fn says(self) -> &'static str {
        match self {
            VarNameWhy::Empty => "is empty",
            VarNameWhy::Equals => "holds a `=`, which would end the name in the environment block",
            VarNameWhy::Nul => "holds a NUL, which would end its entry in the environment block",
            VarNameWhy::OutOfMemory { .. } => "found no room on the global heap for its text",
        }
    }
}

impl fmt::Display for VarNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            VarNameWhy::OutOfMemory { asked } => write!(
                f,
                "an environment variable name of {asked} bytes {}",
                self.why.says()
            ),
            why => write!(f, "environment variable name {:?} {}", self.name, why.says()),
        }
    }
}

impl core::error::Error for VarNameError {}

/// Why `raw` is no variable name, if it is not. A `const fn`, so [`crate::var!`] checks a
/// literal as the crate compiles with the same rule [`VarName::new`] checks at run time.
pub const fn refused(raw: &str) -> Option<VarNameWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(VarNameWhy::Empty);
    }
    let mut rest = bytes;
    while let [byte, more @ ..] = rest {
        match *byte {
            b'=' => return Some(VarNameWhy::Equals),
            0 => return Some(VarNameWhy::Nul),
            _ => {}
        }
        rest = more;
    }
    None
}


impl VarName {
    pub fn new(raw: &str) -> Result<VarName, VarNameError> {
        let out_of_memory = || VarNameError {
            name: String::new(),
            why: VarNameWhy::OutOfMemory { asked: raw.len() },
        };
        if let Some(why) = refused(raw) {
            // alloc: an error's name of what it refused.
            let name = heap::string(raw).map_err(|_| out_of_memory())?;
            return Err(VarNameError { name, why });
        }
        // alloc: once per name, its text.
        let text = heap::string(raw).map_err(|_| out_of_memory())?;
        let text = heap::shared(text).map_err(|_| out_of_memory())?;
        Ok(VarName(Text::Made(text)))
    }

    /// A literal checked by the rule, or why it is none: what [`crate::var!`] evaluates in a
    /// `const`, where a refusal is the compile error that says why. A [`Literal`] is `Copy`,
    /// so the `const` drops nothing.
    #[doc(hidden)]
    pub const fn literal(raw: &'static str) -> Result<Literal, &'static str> {
        match refused(raw) {
            Some(why) => Err(why.says()),
            None => Ok(Literal(raw)),
        }
    }

    /// The name of a checked literal: it keeps the literal itself, so it allocates nothing.
    #[doc(hidden)]
    pub const fn of_literal(literal: Literal) -> VarName {
        VarName(Text::Literal(literal.0))
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Text::Literal(text) => text,
            Text::Made(text) => text.as_str(),
        }
    }
}

/// Names are equal, ordered and hashed by their text, however it is held.
impl PartialEq for VarName {
    fn eq(&self, other: &VarName) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for VarName {}

impl PartialOrd for VarName {
    fn partial_cmp(&self, other: &VarName) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VarName {
    fn cmp(&self, other: &VarName) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for VarName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
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

/// A literal [`VarName::literal`] checked: the one way to [`VarName::of_literal`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Literal(&'static str);

impl TryFrom<&str> for VarName {
    type Error = VarNameError;

    fn try_from(raw: &str) -> Result<VarName, VarNameError> {
        VarName::new(raw)
    }
}

/// A [`VarName`] from a string literal, checked as the crate compiles: `var!("HOME")`. A bad
/// literal (`var!("A=B")`) is a compile error saying why.
#[macro_export]
macro_rules! var {
    ($raw:literal) => {{
        const NAME: $crate::env::VarName = match $crate::env::VarName::literal($raw) {
            Ok(literal) => $crate::env::VarName::of_literal(literal),
            Err(why) => panic!("{}", why),
        };
        NAME
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
    vars: Arc<alloc::vec::Vec<(String, String)>>,
    /// The names it may read; `None` for every one.
    allowed: Option<Arc<Vec<VarName, Global>>>,
    plant: Plant,
}

impl Env {
    pub(crate) fn new(vars: Arc<alloc::vec::Vec<(String, String)>>, plant: Plant) -> Env {
        Env { vars, allowed: None, plant }
    }

    /// The variable `name`: its value, `None` when it is not set, or [`NotAllowed`] when
    /// this may not read it. The two are never one answer: a program that was narrowed away
    /// from a variable does not take it for unset.
    pub fn get(&self, name: &VarName) -> Result<Option<&str>, NotAllowed> {
        if !self.may_read(name) {
            // clone: an error's name of its variable, a refcount.
            return Err(NotAllowed { name: name.clone() });
        }
        Ok(self.vars.iter().find(|(it, _)| it.as_str() == name.as_str()).map(|(_, value)| value.as_str()))
    }

    /// Whether this may read `name`, set or not.
    pub fn may_read(&self, name: &VarName) -> bool {
        self.may_read_str(name.as_str())
    }

    fn may_read_str(&self, name: &str) -> bool {
        self.allowed.as_deref().is_none_or(|allowed| allowed.iter().any(|it| it.as_str() == name))
    }

    /// Only the variables of `names` this may read already: never one more.
    /// [`Stopped::OutOfMemory`] when the global heap has no room for the names kept.
    pub fn only(&self, names: &[VarName]) -> Result<Env, Stopped> {
        let kept = names.iter().filter(|name| match self.plant {
            Plant::EnvOnlyReplaces => true,
            _ => self.may_read(name),
        });
        // alloc: once per narrowing, the names kept (each a refcount).
        let allowed = heap::shared(Vec::from_iter_in(kept.cloned(), Global)?)?;
        Ok(Env { vars: Arc::clone(&self.vars), allowed: Some(allowed), plant: self.plant })
    }

    /// Each variable this may read that is set, in the world's order.
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
