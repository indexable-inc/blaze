//! Loading an extension into this process: [`load`] for code blaze compiled
//! ([`crate::build::Image`]), [`Loaded::linked`] for one compiled into the binary (blaze's
//! own, and the tests').
//!
//! The code is mapped with [`blaze_inproc::exec::Executable`], the memory blaze already runs
//! machine code it made in: written once at the addresses it runs at, then read-and-execute,
//! never writable again, unmapped when dropped. A [`Loaded`] owns the mapping, and a job
//! borrows the `Loaded` for as long as it runs, so the code is unmapped only when no job can
//! be in it: no fence is needed, since a job never outlives its call.
//!
//! An edit of an extension's source is a new key, so a new image, loaded beside the old one;
//! the old one is dropped when the daemon's table stops naming it and its last job ends.
//! Patching one function of a loaded extension in place (blaze-live's slots) is slice 3.

use std::fmt;

use blaze_ext::{Extension, Level};
use blaze_ids::ContentId;

use crate::build::{Image, Imports, Keep};

/// An extension ready to run.
pub struct Loaded {
    name: String,
    /// The build's key: the extension's code identity in every key of what it computes.
    key: ContentId,
    extension: Extension,
    /// The facts in an order where each comes after those it requires.
    fact_order: Vec<usize>,
    /// The mapped code, for one blaze compiled; dropped after everything that points into it.
    _mapped: Option<Mapped>,
}

/// What keeps a loaded image's code alive.
struct Mapped {
    _keep: Keep,
    _code: blaze_inproc::exec::Executable,
}

/// Why an extension was not loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The image's entry is past its code.
    Entry { extension: String, entry: usize, code_len: usize },
    /// The executable memory could not be made, or the image could not be placed in it.
    Map { extension: String, why: String },
    /// `extension()` panicked.
    EntryPanicked { extension: String },
    /// What `extension()` returned is not an extension this blaze can run: `why` names the
    /// lint or fact and the rule.
    Descriptor { extension: String, why: String },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Entry { extension, entry, code_len } => write!(
                f,
                "extension `{extension}`: its image's entry is at {entry}, past its {code_len} bytes of code"
            ),
            LoadError::Map { extension, why } => {
                write!(f, "extension `{extension}`: its code could not be mapped: {why}")
            }
            LoadError::EntryPanicked { extension } => {
                write!(f, "extension `{extension}`: its `extension()` panicked")
            }
            LoadError::Descriptor { extension, why } => write!(f, "extension `{extension}`: {why}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Maps `image` executable in this process and asks it what it is. `name` is the name the
/// workspace gives the extension, which its own must equal.
pub fn load(image: Image, name: &str) -> Result<Loaded, LoadError> {
    let Image { key, code_len, data_len, entry, place } = image;
    if entry >= code_len {
        // alloc: error path
        return Err(LoadError::Entry { extension: name.to_owned(), entry, code_len });
    }
    let imports = Imports {
        fuel_cell: blaze_ext::host::fuel_cell as *const () as usize,
        fuel_out: blaze_ext::host::fuel_out as *const () as usize,
    };
    let mut placed = None;
    let code = blaze_inproc::exec::Executable::written(code_len, data_len, |code, data| {
        placed = Some(place(code, data, &imports));
    })
    // alloc: error path
    .map_err(|error| LoadError::Map { extension: name.to_owned(), why: error.what })?;
    let keep = match placed {
        Some(Ok(keep)) => keep,
        // alloc: error path
        Some(Err(why)) => return Err(LoadError::Map { extension: name.to_owned(), why }),
        None => {
            return Err(LoadError::Map {
                extension: name.to_owned(),                   // alloc: error path
                why: "the image was never placed".to_owned(), // alloc: error path
            });
        }
    };
    let address = code.code_address() as usize + entry;
    // SAFETY: `address` is the start of `extension` in code this blaze compiled from a source
    // that passed `check::check`, under the key `build_key` gave it: a safe Rust function
    // `fn() -> blaze_ext::Extension`, compiled by the same compiler against the same
    // `blaze-ext` this crate is linked with, so its ABI and `Extension`'s layout are this
    // binary's. The upholding is the daemon's `Toolchain` (it builds only checked sources and
    // says where `ENTRY` is) and, for layout, that this blaze was built by its own source
    // (docs/v2/plan/design/extensions.md, "Types match by construction"). The code stays
    // mapped while `Loaded` lives: `Mapped` holds it and is dropped last.
    let entry: fn() -> Extension =
        unsafe { std::mem::transmute::<usize, fn() -> Extension>(address) };
    let extension = std::panic::catch_unwind(entry)
        // alloc: error path
        .map_err(|_| LoadError::EntryPanicked { extension: name.to_owned() })?;
    let mut loaded = Loaded::described(extension, key, name)?;
    loaded._mapped = Some(Mapped { _keep: keep, _code: code });
    Ok(loaded)
}

impl Loaded {
    /// An extension compiled into this binary: `extension` is what its `extension()`
    /// returned, `key` its code identity (for a test, any; for one of blaze's own, blaze's).
    pub fn linked(extension: Extension, key: ContentId) -> Result<Loaded, LoadError> {
        Loaded::described(extension, key, extension.name)
    }

    /// Holds what `extension()` returned to the rules a descriptor has: the API's version,
    /// the name the workspace gave, names that are identifiers and are not used twice, and
    /// `requires` that name the extension's own facts with no cycle among them.
    fn described(extension: Extension, key: ContentId, name: &str) -> Result<Loaded, LoadError> {
        // alloc: error path
        let wrong = |why: String| LoadError::Descriptor { extension: name.to_owned(), why };
        if extension.api != blaze_ext::VERSION {
            return Err(wrong(format!(
                "it was compiled against blaze-ext {}, and this blaze has blaze-ext {}",
                extension.api,
                blaze_ext::VERSION
            )));
        }
        if extension.name != name {
            return Err(wrong(format!(
                "its `extension()` names itself `{}`, not the name the workspace gives it",
                extension.name
            )));
        }
        let fact = |name: &str| extension.facts.iter().position(|it| it.name == name);
        for (at, lint) in extension.lints.iter().enumerate() {
            if !is_name(lint.name) {
                return Err(wrong(format!(
                    "the lint `{}`: a lint's name is a lowercase identifier",
                    lint.name
                )));
            }
            if extension.lints[..at].iter().any(|it| it.name == lint.name) {
                return Err(wrong(format!("the lint `{}` is declared twice", lint.name)));
            }
            if let Some(missing) = lint.requires.iter().find(|it| fact(it).is_none()) {
                return Err(wrong(format!(
                    "the lint `{}` requires the fact `{missing}`, which the extension does not declare",
                    lint.name
                )));
            }
        }
        for (at, decl) in extension.facts.iter().enumerate() {
            if !is_name(decl.name) {
                return Err(wrong(format!(
                    "the fact `{}`: a fact's name is a lowercase identifier",
                    decl.name
                )));
            }
            if extension.facts[..at].iter().any(|it| it.name == decl.name) {
                return Err(wrong(format!("the fact `{}` is declared twice", decl.name)));
            }
            if let Some(missing) = decl.requires.iter().find(|it| fact(it).is_none()) {
                return Err(wrong(format!(
                    "the fact `{}` requires the fact `{missing}`, which the extension does not declare",
                    decl.name
                )));
            }
        }
        // The facts in dependency order: each round takes those whose requirements are all
        // taken, at most one round per fact; a round that takes none is a cycle.
        let count = extension.facts.len();
        let mut fact_order = Vec::with_capacity(count); // alloc: once per load
        let mut taken = vec![false; count]; // alloc: once per load
        for _ in 0..count {
            let before = fact_order.len();
            for (at, decl) in extension.facts.iter().enumerate() {
                let ready = decl
                    .requires
                    .iter()
                    .all(|it| *it == decl.name || fact(it).is_some_and(|it| taken[it]));
                if !taken[at] && ready {
                    fact_order.push(at);
                }
            }
            for &at in &fact_order[before..] {
                taken[at] = true;
            }
            if fact_order.len() == before {
                break;
            }
        }
        if let Some(stuck) = (0..count).find(|at| !taken[*at]) {
            return Err(wrong(format!(
                "the fact `{}` requires itself through other facts: `requires` has a cycle",
                extension.facts[stuck].name
            )));
        }
        // alloc: the name, once per load
        Ok(Loaded { name: name.to_owned(), key, extension, fact_order, _mapped: None })
    }

    /// As the workspace names it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its code identity.
    pub fn key(&self) -> ContentId {
        self.key
    }

    /// What its `extension()` returned. For an image blaze mapped, its slices and function
    /// pointers point into that image, whatever their `'static` says: they are valid while
    /// `self` is, so it is lent, and a caller copies nothing of it out past its borrow of
    /// `self` (`run` holds the `Loaded` across every job it starts).
    pub fn extension(&self) -> &Extension {
        &self.extension
    }

    /// Its facts' indices, each after those it requires.
    pub fn fact_order(&self) -> &[usize] {
        &self.fact_order
    }

    /// The name its lint `lint` is reported under: `panic_in_drop::panic_in_drop` for the
    /// lint `panic_in_drop` of `panic-in-drop`, the form `#[allow(..)]` can name.
    pub fn lint_name(&self, lint: usize) -> String {
        format!("{}::{}", self.name.replace('-', "_"), self.extension.lints[lint].name)
    }

    /// The level of its lint `lint`: the workspace's when it sets one, else the lint's own.
    pub fn level(&self, lint: usize, levels: &[(String, Level)]) -> Level {
        let own = &self.extension.lints[lint];
        levels.iter().find(|(name, _)| name == own.name).map_or(own.level, |(_, level)| *level)
    }
}

/// Whether `name` is a lowercase identifier: what a lint's and a fact's name is.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|it| it.is_ascii_lowercase())
        && chars.all(|it| it.is_ascii_lowercase() || it.is_ascii_digit() || it == '_')
}
