//! Turning data into instructions (docs/v2/plan/design/std.md, "The rest of the set", `Code`):
//! make a region of memory executable, and load a module of native code. What live patching
//! and a JIT hold; nothing else can do either. Narrowed to one module by [`Code::only`], which
//! never widens: a narrowed one loads that module and makes nothing else executable.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::modules`]), which records what
//! was loaded and made executable and runs nothing: blaze-core has no effect that changes a
//! mapping's protection or loads a library.

use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    name::ModuleName,
    narrow::Allowed,
};

/// What a call on code was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeOp {
    Executable,
    Load,
}

/// A call on code that failed: the call, the module (none for a region) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct CodeError {
    pub op: CodeOp,
    pub module: Option<ModuleName>,
    pub kind: Refused,
}

impl fmt::Display for CodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.module {
            Some(module) => write!(f, "bstd::code {:?} of module {module}: {}", self.op, self.kind),
            None => write!(f, "bstd::code {:?} of a region of memory: {}", self.op, self.kind),
        }
    }
}

impl core::error::Error for CodeError {}

/// A module that was loaded.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount
pub struct Module {
    name: ModuleName,
}

impl Module {
    pub fn name(&self) -> &ModuleName {
        &self.name
    }
}

/// Making code: from [`Io::code`].
#[derive(Clone)]
pub struct Code {
    io: Io,
    allowed: Allowed<ModuleName>,
}

impl Code {
    pub(crate) fn new(io: Io) -> Code {
        Code { io, allowed: Allowed::Any }
    }

    /// Only the module `module`, if this reaches it already; and no region of memory.
    pub fn only(&self, module: &ModuleName) -> Code {
        Code { io: self.io.clone(), allowed: self.allowed.one(module, self.io.plant()) }
    }

    /// The region `code` made executable. Refused by a `Code` narrowed to a module.
    pub fn executable(&self, cx: &Cx, code: &[u8]) -> Result<(), CodeError> {
        let refuse = |kind| CodeError { op: CodeOp::Executable, module: None, kind };
        if !self.allowed.whole() {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            host.executable.push(code.len());
            Ok(())
        })
        .map_err(refuse)
    }

    /// The module `module`, loaded.
    pub fn load(&self, cx: &Cx, module: &ModuleName) -> Result<Module, CodeError> {
        // clone: an error's name of its module, a refcount.
        let refuse = |kind| CodeError { op: CodeOp::Load, module: Some(module.clone()), kind };
        if !self.allowed.allows(module) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            if !host.modules.contains(module) {
                return Err(Refused::NotFound);
            }
            // clone: the simulated host's record and the module's name, refcounts.
            host.loaded.push(module.clone());
            Ok(Module { name: module.clone() })
        })
        .map_err(refuse)
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Code({:?})", self.allowed)
    }
}
